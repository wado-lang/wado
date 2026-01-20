//! Type resolution phase for Wado
//!
//! The type resolver:
//! 1. Takes the desugared AST and symbol table from the analyzer
//! 2. Performs type inference and type checking
//! 3. Produces the Typed Intermediate Representation (TIR)
//!
//! All type resolution happens in this phase. The output TIR has fully
//! resolved types on every expression, making code generation mechanical.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;

use indexmap::IndexMap;

use crate::ast::{
    self, BinaryOp, Block, BreakStmt, ContinueStmt, Expr, ExprStmt, ForOfStmt, ForStmt, Function,
    IfExpr, IfStmt, Item, LetStmt, Literal, LoopStmt, MatchArm, Module, Pattern, ReturnStmt, Stmt,
    Type, UnaryOp, WhileStmt,
};
use crate::project::Project;
use crate::symbol::{SymbolKind, SymbolTable};
use crate::tir::{
    FunctionRef, MonomorphInfo, ResolvedType, SubstitutionContext, TirBinaryOp, TirBlock,
    TirCapture, TirExpr, TirExprKind, TirFunction, TirLiteralPattern, TirMatchArm, TirModule,
    TirParam, TirPattern, TirStmt, TirStmtKind, TirStruct, TirStructField, TirUnaryOp,
    TirVariantCase, TirVariantDecl, TypeId, TypeTable,
};
use crate::token::Span;

/// Module path for the String struct in core/prelude
const STRING_MODULE_PATH: &[&str] = &["core", "prelude"];

/// Helper to convert STRING_MODULE_PATH to Vec<String>
fn string_module_path() -> Vec<String> {
    STRING_MODULE_PATH
        .iter()
        .map(|s| (*s).to_string())
        .collect()
}

/// Struct field info: (module_path, fields) where fields is a list of (name, type_id) pairs
type StructFieldInfo = (Vec<String>, Vec<(String, TypeId)>);

/// Variant case info: case_name -> field_type_ids
type VariantCaseData = (String, Vec<TypeId>);
/// Variant info: (module_path, type_params, cases)
type VariantInfo = (Vec<String>, Vec<String>, Vec<VariantCaseData>);

/// Errors from the type resolution phase
#[derive(Debug, Clone)]
pub enum TypeError {
    /// Type mismatch
    TypeMismatch {
        expected: String,
        found: String,
        span: Span,
    },

    /// Unknown type name
    UnknownType { name: String, span: Span },

    /// Unknown function
    UnknownFunction { name: String, span: Span },

    /// Unknown variable
    UnknownVariable { name: String, span: Span },

    /// Field not found on struct
    FieldNotFound {
        struct_name: String,
        field_name: String,
        span: Span,
    },

    /// Wrong number of arguments
    ArgumentCountMismatch {
        expected: usize,
        found: usize,
        span: Span,
    },

    /// Invalid numeric literal
    InvalidLiteral { message: String, span: Span },

    /// Feature not yet implemented
    NotYetImplemented { feature: String, span: Span },

    /// Invalid assignment target (not a valid l-value)
    CannotAssign { message: String, span: Span },
}

impl std::fmt::Display for TypeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TypeError::TypeMismatch {
                expected,
                found,
                span,
            } => {
                write!(
                    f,
                    "{}:{}: type mismatch: expected '{}', found '{}'",
                    span.line, span.column, expected, found
                )
            }
            TypeError::UnknownType { name, span } => {
                write!(f, "{}:{}: unknown type '{}'", span.line, span.column, name)
            }
            TypeError::UnknownFunction { name, span } => {
                write!(
                    f,
                    "{}:{}: unknown function '{}'",
                    span.line, span.column, name
                )
            }
            TypeError::UnknownVariable { name, span } => {
                write!(
                    f,
                    "{}:{}: unknown variable '{}'",
                    span.line, span.column, name
                )
            }
            TypeError::FieldNotFound {
                struct_name,
                field_name,
                span,
            } => {
                write!(
                    f,
                    "{}:{}: field '{}' not found on struct '{}'",
                    span.line, span.column, field_name, struct_name
                )
            }
            TypeError::ArgumentCountMismatch {
                expected,
                found,
                span,
            } => {
                write!(
                    f,
                    "{}:{}: expected {} arguments, found {}",
                    span.line, span.column, expected, found
                )
            }
            TypeError::InvalidLiteral { message, span } => {
                write!(f, "{}:{}: {}", span.line, span.column, message)
            }
            TypeError::NotYetImplemented { feature, span } => {
                write!(
                    f,
                    "{}:{}: {} is not yet implemented",
                    span.line, span.column, feature
                )
            }
            TypeError::CannotAssign { message, span } => {
                write!(
                    f,
                    "{}:{}: cannot assign: {}",
                    span.line, span.column, message
                )
            }
        }
    }
}

impl std::error::Error for TypeError {}

/// Local variable information during resolution
#[derive(Debug, Clone)]
struct LocalVar {
    #[allow(dead_code)] // For debugging
    name: String,
    type_id: TypeId,
    index: u32,
    #[allow(dead_code)] // For future mutability checking
    is_mut: bool,
}

/// Method lookup result including return type and self parameter kind
#[derive(Debug, Clone, Copy)]
struct MethodInfo {
    return_type: TypeId,
    self_kind: ast::SelfKind,
}

/// Function context during resolution with scope tracking
struct FunctionContext {
    /// Stack of scopes (each scope maps name -> LocalVar)
    scopes: Vec<HashMap<String, LocalVar>>,
    /// Next local index (Wasm locals are function-wide)
    next_local: u32,
    /// Return type of the function
    #[allow(dead_code)] // For future return type checking
    return_type: TypeId,
    /// Local variable types in order (for Wasm local declarations)
    local_types: Vec<TypeId>,
    /// Local indices that have their address taken (&x or &mut x)
    address_taken_locals: HashSet<u32>,
    /// Outer context locals for closure capture detection (name -> LocalVar snapshot)
    /// Only set for closure contexts
    outer_locals: HashMap<String, LocalVar>,
    /// Captured variables detected during resolution (name -> capture index)
    /// Only used for closure contexts
    captured_vars: HashMap<String, u32>,
}

impl FunctionContext {
    fn new(return_type: TypeId) -> Self {
        Self {
            scopes: vec![HashMap::new()], // Start with one scope for function parameters
            next_local: 0,
            return_type,
            local_types: Vec::new(),
            address_taken_locals: HashSet::new(),
            outer_locals: HashMap::new(),
            captured_vars: HashMap::new(),
        }
    }

    /// Create a closure context with outer scope access for capture detection
    fn new_closure(return_type: TypeId, outer_ctx: &FunctionContext) -> Self {
        // Snapshot all locals from outer context
        let mut outer_locals = HashMap::new();
        for scope in &outer_ctx.scopes {
            for (name, local) in scope {
                outer_locals.insert(name.clone(), local.clone());
            }
        }

        Self {
            scopes: vec![HashMap::new()],
            next_local: 0,
            return_type,
            local_types: Vec::new(),
            address_taken_locals: HashSet::new(),
            outer_locals,
            captured_vars: HashMap::new(),
        }
    }

    /// Enter a new scope (for blocks, if/while/for/loop bodies)
    fn enter_scope(&mut self) {
        self.scopes.push(HashMap::new());
    }

    /// Exit the current scope
    fn exit_scope(&mut self) {
        self.scopes.pop();
    }

    /// Add a local variable to the current scope
    fn add_local(&mut self, name: String, type_id: TypeId, is_mut: bool) -> u32 {
        let index = self.next_local;
        self.next_local += 1;
        self.local_types.push(type_id);

        let scope = self.scopes.last_mut().unwrap();
        scope.insert(
            name.clone(),
            LocalVar {
                name,
                type_id,
                index,
                is_mut,
            },
        );
        index
    }

    /// Look up a variable by name (searches from innermost to outermost scope)
    fn lookup(&self, name: &str) -> Option<&LocalVar> {
        for scope in self.scopes.iter().rev() {
            if let Some(local) = scope.get(name) {
                return Some(local);
            }
        }
        None
    }

    /// Look up a variable, checking outer context for captures if in a closure.
    /// Returns either a local variable reference or a capture reference.
    fn lookup_or_capture(&mut self, name: &str) -> Option<VarRef> {
        // First check local scopes
        for scope in self.scopes.iter().rev() {
            if let Some(local) = scope.get(name) {
                return Some(VarRef::Local {
                    index: local.index,
                    type_id: local.type_id,
                });
            }
        }

        // Check outer context (for closures)
        if let Some(outer_local) = self.outer_locals.get(name) {
            // Check if we already captured this variable
            if let Some(&capture_index) = self.captured_vars.get(name) {
                return Some(VarRef::Capture {
                    index: capture_index,
                    type_id: outer_local.type_id,
                });
            }

            // Record a new capture
            let capture_index = self.captured_vars.len() as u32;
            self.captured_vars.insert(name.to_string(), capture_index);

            return Some(VarRef::Capture {
                index: capture_index,
                type_id: outer_local.type_id,
            });
        }

        None
    }

    /// Get the list of captures for building TirCapture entries
    fn get_captures(&self) -> Vec<(String, u32, &LocalVar)> {
        let mut captures: Vec<_> = self
            .captured_vars
            .iter()
            .filter_map(|(name, &index)| {
                self.outer_locals
                    .get(name)
                    .map(|local| (name.clone(), index, local))
            })
            .collect();
        // Sort by capture index for consistent ordering
        captures.sort_by_key(|(_, index, _)| *index);
        captures
    }
}

/// Reference to a variable (either local or captured)
enum VarRef {
    Local { index: u32, type_id: TypeId },
    Capture { index: u32, type_id: TypeId },
}

/// The resolver converts AST to TIR with resolved types
pub struct Resolver<'a> {
    /// Type table (shared across all modules via Rc<RefCell>)
    type_table: Rc<RefCell<TypeTable>>,
    /// Symbol table from analyzer
    #[allow(dead_code)]
    symbols: &'a SymbolTable,
    /// Loaded modules from analyzer
    #[allow(dead_code)]
    loaded_modules: &'a HashMap<Vec<String>, Module>,
    /// Type aliases (name -> resolved type)
    type_aliases: HashMap<String, TypeId>,
    /// Struct field info (struct name -> (module_path, fields))
    struct_fields: HashMap<String, StructFieldInfo>,
    /// Variant case info (variant name -> (module_path, type_params, cases))
    variant_cases: HashMap<String, VariantInfo>,
    /// Function return types (name -> return type)
    function_return_types: HashMap<String, TypeId>,
    /// Imported function names for the current module
    imported_functions: HashSet<String>,
    /// Errors collected during resolution
    errors: Vec<TypeError>,
    /// Current module path being resolved (for struct type module_path)
    current_module_path: Vec<String>,
    /// Current module items (for local function parameter lookup)
    current_module_items: Vec<Item>,
    /// Type parameters currently in scope (name -> (index, TypeId))
    /// Set when resolving generic structs or functions
    current_type_params: HashMap<String, (u32, TypeId)>,
    /// Generic struct definitions (name -> type param count)
    /// Used to determine if a struct is generic
    generic_struct_names: HashSet<String>,
    /// Generic function type parameters (func_name -> type_params)
    /// Used for substituting type parameters in return types
    generic_function_params: HashMap<String, Vec<(String, TypeId)>>,
    /// Generic method type parameters (mangled_name -> type_params)
    /// Used for substituting type parameters in method return types
    generic_method_params: HashMap<String, Vec<(String, TypeId)>>,
}

impl<'a> Resolver<'a> {
    /// Create a new resolver
    pub fn new(
        symbols: &'a SymbolTable,
        loaded_modules: &'a HashMap<Vec<String>, Module>,
    ) -> Self {
        Self {
            type_table: Rc::new(RefCell::new(TypeTable::new())),
            symbols,
            loaded_modules,
            type_aliases: HashMap::new(),
            struct_fields: HashMap::new(),
            variant_cases: HashMap::new(),
            function_return_types: HashMap::new(),
            imported_functions: HashSet::new(),
            errors: Vec::new(),
            current_module_path: Vec::new(),
            current_module_items: Vec::new(),
            current_type_params: HashMap::new(),
            generic_struct_names: HashSet::new(),
            generic_function_params: HashMap::new(),
            generic_method_params: HashMap::new(),
        }
    }

    /// Resolve a module, converting AST to TIR
    pub fn resolve_module(
        &mut self,
        module: &Module,
        module_path: Vec<String>,
    ) -> Result<TirModule, Vec<TypeError>> {
        // Set current module path for struct type creation
        self.current_module_path = module_path.clone();
        // Store current module items for local function parameter lookup
        self.current_module_items = module.items.clone();

        // First pass: collect type definitions
        self.collect_types(module);

        // Second pass: collect function signatures (for call resolution)
        self.collect_function_signatures(module);

        // Third pass: resolve functions
        let mut tir_module = TirModule::new(module_path);

        for item in &module.items {
            match item {
                Item::Function(func) => {
                    if let Some(tir_func) = self.resolve_function(func) {
                        tir_module.add_function(tir_func);
                    }
                }
                Item::Struct(struct_decl) => {
                    let tir_struct = self.resolve_struct(struct_decl);
                    tir_module.add_struct(tir_struct);
                }
                Item::Impl(impl_block) => {
                    // Resolve impl block methods with mangled names
                    let struct_name = self.get_type_name(&impl_block.ty);
                    let trait_name = impl_block
                        .trait_type
                        .as_ref()
                        .map(|t| self.get_type_name(t));
                    for method in &impl_block.methods {
                        if let Some(mut tir_func) = self.resolve_method(
                            method,
                            &struct_name,
                            &impl_block.ty,
                            trait_name.as_deref(),
                        ) {
                            // Mangle the method name:
                            // - Trait impl: StructName^TraitName::method_name
                            // - Inherent impl: StructName::method_name
                            tir_func.name = match &trait_name {
                                Some(trait_n) => {
                                    format!("{}^{}::{}", struct_name, trait_n, method.name)
                                }
                                None => format!("{}::{}", struct_name, method.name),
                            };
                            tir_module.add_function(tir_func);
                        }
                    }
                }
                Item::Trait(_trait_decl) => {
                    // Trait declarations are handled in the first pass (signature registration)
                    // No TIR output needed for trait declarations themselves
                }
                Item::Variant(variant_decl) => {
                    let tir_variant = self.resolve_variant_decl(variant_decl);
                    tir_module.variants.push(tir_variant);
                }
                // Other items will be added as needed
                _ => {}
            }
        }

        // Share the type table via Rc::clone
        tir_module.type_table = Rc::clone(&self.type_table);

        // Preserve data section
        if let Some(data) = module.data_section() {
            tir_module = tir_module.with_data_section(Some(data.to_string()));
        }

        if self.errors.is_empty() {
            Ok(tir_module)
        } else {
            Err(std::mem::take(&mut self.errors))
        }
    }

    /// Resolve all modules to TIR
    ///
    /// This resolves every module (entry and dependencies) to TIR,
    /// enabling TIR-only code generation.
    /// Modules are returned in topological order based on struct field dependencies.
    pub fn resolve_all_modules(
        symbols: &'a SymbolTable,
        modules: &'a HashMap<Vec<String>, Module>,
        entry_path: &[String],
    ) -> Result<IndexMap<Vec<String>, TirModule>, Vec<TypeError>> {
        let mut result = IndexMap::new();
        let mut all_errors = Vec::new();

        // Create a shared type table wrapped in Rc<RefCell<>> for cross-module sharing
        let type_table = Rc::new(RefCell::new(TypeTable::new()));
        let mut type_aliases = HashMap::new();
        let mut struct_fields = HashMap::new();
        let mut variant_cases: HashMap<String, VariantInfo> = HashMap::new();

        // First pass: collect struct and variant names from all modules (for forward references)
        for (path, module) in modules {
            for item in &module.items {
                match item {
                    Item::Struct(struct_decl) => {
                        // Insert with empty fields first - will be populated in second sub-pass
                        struct_fields.insert(struct_decl.name.clone(), (path.clone(), Vec::new()));
                    }
                    Item::Variant(variant_decl) => {
                        // Insert with empty cases first - will be populated in second sub-pass
                        let type_params: Vec<String> = variant_decl
                            .type_params
                            .iter()
                            .map(|p| p.name.clone())
                            .collect();
                        variant_cases.insert(
                            variant_decl.name.clone(),
                            (path.clone(), type_params, Vec::new()),
                        );
                    }
                    _ => {}
                }
            }
        }

        // Second sub-pass: resolve struct fields and type aliases
        for (path, module) in modules {
            for item in &module.items {
                match item {
                    Item::Struct(struct_decl) => {
                        let mut fields = Vec::new();
                        for field in &struct_decl.fields {
                            let type_id = Self::resolve_type_static(
                                &field.ty,
                                &mut type_table.borrow_mut(),
                                &type_aliases,
                                &struct_fields,
                            );
                            fields.push((field.name.clone(), type_id));
                        }
                        // Update the struct_fields entry with actual fields
                        struct_fields.insert(struct_decl.name.clone(), (path.clone(), fields));
                    }
                    Item::Type(type_alias) => {
                        let type_id = Self::resolve_type_static(
                            &type_alias.ty,
                            &mut type_table.borrow_mut(),
                            &type_aliases,
                            &struct_fields,
                        );
                        type_aliases.insert(type_alias.name.clone(), type_id);
                    }
                    Item::Variant(variant_decl) => {
                        // Resolve variant case field types
                        let type_params: Vec<String> = variant_decl
                            .type_params
                            .iter()
                            .map(|p| p.name.clone())
                            .collect();
                        let mut cases = Vec::new();
                        for case in &variant_decl.cases {
                            let field_types = if let Some(fields) = &case.fields {
                                fields
                                    .iter()
                                    .map(|ty| {
                                        Self::resolve_type_static(
                                            ty,
                                            &mut type_table.borrow_mut(),
                                            &type_aliases,
                                            &struct_fields,
                                        )
                                    })
                                    .collect()
                            } else {
                                Vec::new()
                            };
                            cases.push((case.name.clone(), field_types));
                        }
                        variant_cases.insert(
                            variant_decl.name.clone(),
                            (path.clone(), type_params, cases),
                        );
                    }
                    _ => {}
                }
            }
        }

        // Topologically sort modules based on struct field type dependencies
        // A module depends on another if it has a struct with a field of a type defined there
        let sorted_paths =
            Self::topological_sort_modules(modules, &struct_fields, &type_table.borrow());

        // Second pass: resolve each module with per-module function_return_types and imports
        for path in &sorted_paths {
            let module = modules.get(path).expect("module should exist");
            // Build function_return_types for this module only
            // (functions defined in this module)
            let mut function_return_types = HashMap::new();
            for item in &module.items {
                if let Item::Function(func) = item {
                    let return_type = if let Some(ret_ty) = &func.return_type {
                        Self::resolve_type_static(
                            ret_ty,
                            &mut type_table.borrow_mut(),
                            &type_aliases,
                            &struct_fields,
                        )
                    } else {
                        TypeTable::UNIT
                    };
                    function_return_types.insert(func.name.clone(), return_type);
                }
            }

            // Collect imported function names from this module's use declarations
            let mut imported_functions = HashSet::new();
            for item in &module.items {
                if let Item::Use(use_decl) = item {
                    for use_item in &use_decl.items {
                        match use_item {
                            crate::ast::UseItem::Simple { name, alias } => {
                                // Add both original name and alias (if any)
                                imported_functions.insert(name.clone());
                                if let Some(a) = alias {
                                    imported_functions.insert(a.clone());
                                }
                            }
                            crate::ast::UseItem::EffectFunctions { functions, .. } => {
                                // Effect functions are imported by their function name
                                for func_item in functions {
                                    imported_functions.insert(func_item.name.clone());
                                    if let Some(a) = &func_item.alias {
                                        imported_functions.insert(a.clone());
                                    }
                                }
                            }
                        }
                    }
                }
            }

            let mut resolver = Resolver {
                type_table: Rc::clone(&type_table), // Share the same TypeTable via Rc::clone
                symbols,
                loaded_modules: modules,
                type_aliases: type_aliases.clone(),
                struct_fields: struct_fields.clone(),
                variant_cases: variant_cases.clone(),
                function_return_types,
                imported_functions,
                errors: Vec::new(),
                current_module_path: Vec::new(), // Set in resolve_module
                current_module_items: Vec::new(), // Set in resolve_module
                current_type_params: HashMap::new(),
                generic_struct_names: HashSet::new(),
                generic_function_params: HashMap::new(),
                generic_method_params: HashMap::new(),
            };

            match resolver.resolve_module(module, path.clone()) {
                Ok(tir_module) => {
                    // TypeTable is already shared via Rc, no need to merge
                    result.insert(path.clone(), tir_module);
                }
                Err(errors) => {
                    all_errors.extend(errors);
                }
            }
        }

        if all_errors.is_empty() {
            // TypeTable is already shared across all modules via Rc<RefCell<>>
            // No need to unify - all modules point to the same TypeTable
            Ok(result)
        } else {
            Err(all_errors)
        }
    }

    /// Topologically sort modules based on struct field type dependencies.
    ///
    /// A module A depends on module B if A contains a struct with a field whose type
    /// is a struct defined in B. This ensures that when we register struct types in
    /// codegen, dependency structs are registered before the structs that reference them.
    fn topological_sort_modules(
        modules: &HashMap<Vec<String>, Module>,
        struct_fields: &HashMap<String, StructFieldInfo>,
        type_table: &TypeTable,
    ) -> Vec<Vec<String>> {
        // Collect and sort paths for deterministic ordering
        let mut paths: Vec<&Vec<String>> = modules.keys().collect();
        paths.sort();
        let path_to_idx: HashMap<&Vec<String>, usize> =
            paths.iter().enumerate().map(|(i, p)| (*p, i)).collect();

        // Track dependency counts directly (no need for full dependency sets)
        let mut dependency_count: Vec<usize> = vec![0; paths.len()];
        // Track which edges we've already added to avoid duplicates
        let mut seen_edges: HashSet<(usize, usize)> = HashSet::new();
        // Build reverse graph: dependents[i] = modules that depend on module i
        let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); paths.len()];

        // Analyze struct fields to find cross-module dependencies
        for (struct_name, (module_path, fields)) in struct_fields {
            let Some(&from_idx) = path_to_idx.get(module_path) else {
                continue;
            };
            for (_field_name, field_type_id) in fields {
                if let ResolvedType::Struct {
                    name: ref_struct_name,
                    module_path: ref_module_path,
                } = type_table.get(*field_type_id)
                {
                    // Skip self-references (same struct or same module)
                    if ref_struct_name == struct_name || ref_module_path == module_path {
                        continue;
                    }
                    if let Some(&to_idx) = path_to_idx.get(ref_module_path) {
                        // from_idx depends on to_idx (dependency edge)
                        if seen_edges.insert((from_idx, to_idx)) {
                            dependency_count[from_idx] += 1;
                            dependents[to_idx].push(from_idx);
                        }
                    }
                }
            }
        }

        // Kahn's algorithm: start with modules that have no dependencies
        let mut queue: VecDeque<usize> = dependency_count
            .iter()
            .enumerate()
            .filter(|(_, count)| **count == 0)
            .map(|(i, _)| i)
            .collect();

        let mut sorted_indices = Vec::with_capacity(paths.len());
        while let Some(idx) = queue.pop_front() {
            sorted_indices.push(idx);
            // Update dependents using reverse graph (O(1) per edge)
            for &dependent_idx in &dependents[idx] {
                dependency_count[dependent_idx] -= 1;
                if dependency_count[dependent_idx] == 0 {
                    queue.push_back(dependent_idx);
                }
            }
        }

        // Cycle detection with warning (O(n) using HashSet)
        if sorted_indices.len() < paths.len() {
            let sorted_set: HashSet<usize> = sorted_indices.iter().copied().collect();
            let in_cycle: Vec<usize> = (0..paths.len())
                .filter(|i| !sorted_set.contains(i))
                .collect();
            let cycle_modules: Vec<_> = in_cycle.iter().map(|&i| paths[i].join("::")).collect();
            eprintln!(
                "Warning: circular struct dependencies detected among modules: {}",
                cycle_modules.join(", ")
            );
            // Append remaining in deterministic order (already sorted by index)
            sorted_indices.extend(in_cycle);
        }

        // Convert indices back to paths
        sorted_indices.iter().map(|&i| paths[i].clone()).collect()
    }

    /// Static version of resolve_type for use before the resolver is fully constructed
    fn resolve_type_static(
        ty: &Type,
        type_table: &mut TypeTable,
        type_aliases: &HashMap<String, TypeId>,
        struct_fields: &HashMap<String, StructFieldInfo>,
    ) -> TypeId {
        match ty {
            Type::Named(named) => {
                // Check type aliases first
                if let Some(&alias_type_id) = type_aliases.get(&named.name) {
                    return alias_type_id;
                }

                // Built-in primitives
                match named.name.as_str() {
                    "bool" => TypeTable::BOOL,
                    "char" => TypeTable::CHAR,
                    "i8" => TypeTable::I8,
                    "i16" => TypeTable::I16,
                    "i32" => TypeTable::I32,
                    "i64" => TypeTable::I64,
                    "u8" => TypeTable::U8,
                    "u16" => TypeTable::U16,
                    "u32" => TypeTable::U32,
                    "u64" => TypeTable::U64,
                    "f32" => TypeTable::F32,
                    "f64" => TypeTable::F64,
                    "()" => TypeTable::UNIT,
                    "!" => TypeTable::NEVER,
                    _ => {
                        // Check if it's a struct type
                        if let Some((module_path, _)) = struct_fields.get(&named.name) {
                            type_table.make_struct(named.name.clone(), module_path.clone())
                        } else {
                            TypeTable::UNKNOWN
                        }
                    }
                }
            }
            Type::Generic(generic) => match generic.name.as_str() {
                "Option" if !generic.args.is_empty() => {
                    let inner = Self::resolve_type_static(
                        &generic.args[0],
                        type_table,
                        type_aliases,
                        struct_fields,
                    );
                    type_table.intern(ResolvedType::Option(inner))
                }
                "Result" if generic.args.len() >= 2 => {
                    let ok = Self::resolve_type_static(
                        &generic.args[0],
                        type_table,
                        type_aliases,
                        struct_fields,
                    );
                    let err = Self::resolve_type_static(
                        &generic.args[1],
                        type_table,
                        type_aliases,
                        struct_fields,
                    );
                    type_table.intern(ResolvedType::Result { ok, err })
                }
                _ => {
                    // Check if it's a generic struct type
                    if let Some((module_path, _)) = struct_fields.get(&generic.name) {
                        // Resolve type arguments
                        let type_args: Vec<TypeId> = generic
                            .args
                            .iter()
                            .map(|arg| {
                                Self::resolve_type_static(
                                    arg,
                                    type_table,
                                    type_aliases,
                                    struct_fields,
                                )
                            })
                            .collect();
                        type_table.make_generic_instance(
                            generic.name.clone(),
                            module_path.clone(),
                            type_args,
                        )
                    } else {
                        TypeTable::UNKNOWN
                    }
                }
            },
            Type::Reference(inner) => {
                let inner_type =
                    Self::resolve_type_static(inner, type_table, type_aliases, struct_fields);
                type_table.make_ref(inner_type)
            }
            Type::MutReference(inner) => {
                let inner_type =
                    Self::resolve_type_static(inner, type_table, type_aliases, struct_fields);
                type_table.make_mut_ref(inner_type)
            }
            Type::NamespacedGeneric(namespaced) => {
                // Handle builtin::array<T>
                if namespaced.namespace == "builtin"
                    && namespaced.name == "array"
                    && let Some(elem_ty) = namespaced.args.first()
                {
                    let elem =
                        Self::resolve_type_static(elem_ty, type_table, type_aliases, struct_fields);
                    return type_table.make_builtin_array(elem);
                }
                TypeTable::UNKNOWN
            }
            _ => TypeTable::UNKNOWN,
        }
    }

    /// Collect type definitions from the module
    fn collect_types(&mut self, module: &Module) {
        // First, collect types from loaded modules (so aliases like Instant = u64 are available)
        for loaded_module in self.loaded_modules.values() {
            for item in &loaded_module.items {
                if let Item::Type(type_alias) = item {
                    // Only add if not already present (main module takes priority)
                    if !self.type_aliases.contains_key(&type_alias.name) {
                        let type_id = self.resolve_type(&type_alias.ty);
                        self.type_aliases.insert(type_alias.name.clone(), type_id);
                    }
                }
            }
        }

        // First pass: collect generic struct names from loaded modules (needed for resolve_generic_type)
        for loaded_module in self.loaded_modules.values() {
            for item in &loaded_module.items {
                if let Item::Struct(struct_decl) = item
                    && !struct_decl.type_params.is_empty()
                {
                    self.generic_struct_names.insert(struct_decl.name.clone());
                }
            }
        }

        // Also collect generic struct names from the current module
        for item in &module.items {
            if let Item::Struct(struct_decl) = item
                && !struct_decl.type_params.is_empty()
            {
                self.generic_struct_names.insert(struct_decl.name.clone());
            }
        }

        // Then collect struct fields from the main module
        for item in &module.items {
            match item {
                Item::Struct(struct_decl) => {
                    // Set up type parameters in scope for resolving field types
                    let old_type_params = std::mem::take(&mut self.current_type_params);
                    for (index, param) in struct_decl.type_params.iter().enumerate() {
                        let type_id = self
                            .type_table
                            .borrow_mut()
                            .make_type_param(param.name.clone(), index as u32);
                        self.current_type_params
                            .insert(param.name.clone(), (index as u32, type_id));
                    }

                    let mut fields = Vec::new();
                    for field in &struct_decl.fields {
                        let type_id = self.resolve_type(&field.ty);
                        fields.push((field.name.clone(), type_id));
                    }
                    self.struct_fields.insert(
                        struct_decl.name.clone(),
                        (self.current_module_path.clone(), fields),
                    );

                    // Restore type params scope
                    self.current_type_params = old_type_params;
                }
                Item::Type(type_alias) => {
                    let type_id = self.resolve_type(&type_alias.ty);
                    self.type_aliases.insert(type_alias.name.clone(), type_id);
                }
                Item::Variant(variant_decl) => {
                    // Set up type parameters in scope for resolving field types
                    let old_type_params = std::mem::take(&mut self.current_type_params);
                    for (index, param) in variant_decl.type_params.iter().enumerate() {
                        let type_id = self
                            .type_table
                            .borrow_mut()
                            .make_type_param(param.name.clone(), index as u32);
                        self.current_type_params
                            .insert(param.name.clone(), (index as u32, type_id));
                    }

                    // Collect type parameters
                    let type_params: Vec<String> = variant_decl
                        .type_params
                        .iter()
                        .map(|p| p.name.clone())
                        .collect();

                    // Collect variant cases with resolved field types
                    let mut cases = Vec::new();
                    for case in &variant_decl.cases {
                        let field_types: Vec<TypeId> = case
                            .fields
                            .as_ref()
                            .map(|fields| fields.iter().map(|f| self.resolve_type(f)).collect())
                            .unwrap_or_default();
                        cases.push((case.name.clone(), field_types));
                    }

                    self.variant_cases.insert(
                        variant_decl.name.clone(),
                        (self.current_module_path.clone(), type_params, cases),
                    );

                    // Restore type params scope
                    self.current_type_params = old_type_params;
                }
                _ => {}
            }
        }
    }

    /// Collect function signatures for call resolution
    fn collect_function_signatures(&mut self, module: &Module) {
        for item in &module.items {
            match item {
                Item::Function(func) => {
                    let return_type = func
                        .return_type
                        .as_ref()
                        .map(|t| self.resolve_type(t))
                        .unwrap_or(TypeTable::UNIT);
                    self.function_return_types
                        .insert(func.name.clone(), return_type);
                }
                Item::Impl(impl_block) => {
                    // Set up type parameters from impl block before resolving method signatures
                    let old_type_params = std::mem::take(&mut self.current_type_params);

                    // First, collect explicit type params from impl<T>
                    for (index, param) in impl_block.type_params.iter().enumerate() {
                        let type_id = self
                            .type_table
                            .borrow_mut()
                            .make_type_param(param.name.clone(), index as u32);
                        self.current_type_params
                            .insert(param.name.clone(), (index as u32, type_id));
                    }

                    // Also collect type params from generic type: impl Array<T> {...}
                    // The type args in Array<T> are type parameters
                    if let ast::Type::Generic(generic) = &impl_block.ty {
                        let offset = impl_block.type_params.len();
                        for (i, arg) in generic.args.iter().enumerate() {
                            if let ast::Type::Named(named) = arg {
                                // Check if this looks like a type parameter (single uppercase letter or PascalCase)
                                // In practice, T, U, V etc. are type params
                                let name = &named.name;
                                if !self.current_type_params.contains_key(name) {
                                    let index = (offset + i) as u32;
                                    let type_id = self
                                        .type_table
                                        .borrow_mut()
                                        .make_type_param(name.clone(), index);
                                    self.current_type_params
                                        .insert(name.clone(), (index, type_id));
                                }
                            }
                        }
                    }

                    // Collect method signatures with mangled names
                    let struct_name = self.get_type_name(&impl_block.ty);
                    let trait_name = impl_block
                        .trait_type
                        .as_ref()
                        .map(|t| self.get_type_name(t));

                    for method in &impl_block.methods {
                        let return_type = method
                            .return_type
                            .as_ref()
                            .map(|t| self.resolve_type(t))
                            .unwrap_or(TypeTable::UNIT);

                        // Mangle the method name:
                        // - Trait impl: StructName^TraitName::method_name
                        // - Inherent impl: StructName::method_name
                        let mangled_name = match &trait_name {
                            Some(trait_n) => {
                                format!("{}^{}::{}", struct_name, trait_n, method.name)
                            }
                            None => format!("{}::{}", struct_name, method.name),
                        };
                        self.function_return_types.insert(mangled_name, return_type);
                    }

                    // Restore type parameters
                    self.current_type_params = old_type_params;
                }
                _ => {}
            }
        }
    }

    /// Resolve a struct declaration
    fn resolve_struct(&mut self, struct_decl: &ast::StructDecl) -> TirStruct {
        // Set up type parameters in scope before resolving fields
        let old_type_params = std::mem::take(&mut self.current_type_params);
        for (index, param) in struct_decl.type_params.iter().enumerate() {
            let type_id = self
                .type_table
                .borrow_mut()
                .make_type_param(param.name.clone(), index as u32);
            self.current_type_params
                .insert(param.name.clone(), (index as u32, type_id));
        }

        let mut fields = Vec::new();
        for (index, field) in struct_decl.fields.iter().enumerate() {
            let type_id = self.resolve_type(&field.ty);
            fields.push(crate::tir::TirField {
                name: field.name.clone(),
                type_id,
                index: index as u32,
                span: field.span,
            });
        }

        // Restore previous type params scope
        self.current_type_params = old_type_params;

        // Convert AST type params to TIR type params
        let type_params: Vec<crate::tir::TirTypeParam> = struct_decl
            .type_params
            .iter()
            .enumerate()
            .map(|(i, p)| crate::tir::TirTypeParam {
                name: p.name.clone(),
                bounds: p.bounds.clone(),
                index: i as u32,
            })
            .collect();

        TirStruct {
            name: struct_decl.name.clone(),
            is_pub: struct_decl.is_pub,
            type_params,
            monomorph_info: None, // Not from monomorphization
            fields,
            span: struct_decl.span,
        }
    }

    /// Resolve a variant declaration
    fn resolve_variant_decl(&mut self, variant_decl: &ast::VariantDecl) -> TirVariantDecl {
        // Set up type parameters in scope before resolving field types
        let old_type_params = std::mem::take(&mut self.current_type_params);
        for (index, param) in variant_decl.type_params.iter().enumerate() {
            let type_id = self
                .type_table
                .borrow_mut()
                .make_type_param(param.name.clone(), index as u32);
            self.current_type_params
                .insert(param.name.clone(), (index as u32, type_id));
        }

        // Resolve each case
        let mut cases = Vec::new();
        for (index, case) in variant_decl.cases.iter().enumerate() {
            let fields = if let Some(field_types) = &case.fields {
                field_types.iter().map(|ty| self.resolve_type(ty)).collect()
            } else {
                Vec::new()
            };
            cases.push(TirVariantCase {
                name: case.name.clone(),
                index: index as u32,
                fields,
                span: case.span,
            });
        }

        // Restore previous type params scope
        self.current_type_params = old_type_params;

        // Convert AST type params to TIR type params
        let type_params: Vec<crate::tir::TirTypeParam> = variant_decl
            .type_params
            .iter()
            .enumerate()
            .map(|(i, p)| crate::tir::TirTypeParam {
                name: p.name.clone(),
                bounds: p.bounds.clone(),
                index: i as u32,
            })
            .collect();

        TirVariantDecl {
            name: variant_decl.name.clone(),
            is_pub: variant_decl.is_pub,
            type_params,
            cases,
            span: variant_decl.span,
        }
    }

    /// Resolve a function
    fn resolve_function(&mut self, func: &Function) -> Option<TirFunction> {
        // Set up type parameters in scope before resolving types
        let old_type_params = std::mem::take(&mut self.current_type_params);
        let mut type_param_list = Vec::new();
        for (index, param) in func.type_params.iter().enumerate() {
            let type_id = self
                .type_table
                .borrow_mut()
                .make_type_param(param.name.clone(), index as u32);
            self.current_type_params
                .insert(param.name.clone(), (index as u32, type_id));
            type_param_list.push((param.name.clone(), type_id));
        }

        // Store type parameters for generic functions (for call site substitution)
        if !func.type_params.is_empty() {
            self.generic_function_params
                .insert(func.name.clone(), type_param_list);
        }

        // Resolve return type
        let return_type = func
            .return_type
            .as_ref()
            .map(|t| self.resolve_type(t))
            .unwrap_or(TypeTable::UNIT);

        // Update the function_return_types with the resolved return type
        // (This replaces the potentially incorrect type from static resolution)
        self.function_return_types
            .insert(func.name.clone(), return_type);

        let mut ctx = FunctionContext::new(return_type);

        // Resolve parameters
        let mut params = Vec::new();
        for param in &func.params {
            let type_id = self.resolve_type(&param.ty);
            let index = ctx.add_local(param.name.clone(), type_id, false);
            params.push(TirParam {
                name: param.name.clone(),
                type_id,
                local_index: index,
                span: param.span,
            });
        }

        // Resolve body
        let body = func.body.as_ref().map(|b| self.resolve_block(b, &mut ctx));

        // Restore previous type params scope
        self.current_type_params = old_type_params;

        // Convert AST type params to TIR type params
        let type_params: Vec<crate::tir::TirTypeParam> = func
            .type_params
            .iter()
            .enumerate()
            .map(|(i, p)| crate::tir::TirTypeParam {
                name: p.name.clone(),
                bounds: p.bounds.clone(),
                index: i as u32,
            })
            .collect();

        Some(TirFunction {
            name: func.name.clone(),
            is_pub: func.is_pub,
            type_params,
            impl_type_params: vec![], // Not a method, no impl type params
            monomorph_info: None,     // Not from monomorphization
            params,
            return_type,
            effects: func.effects.clone(),
            body,
            span: func.span,
            local_count: ctx.next_local,
            local_types: ctx.local_types,
            address_taken_locals: ctx.address_taken_locals,
        })
    }

    /// Resolve a method (function with &self parameter)
    fn resolve_method(
        &mut self,
        func: &Function,
        struct_name: &str,
        impl_type: &Type,
        trait_name: Option<&str>,
    ) -> Option<TirFunction> {
        // Set up type parameters in scope before resolving types
        let old_type_params = std::mem::take(&mut self.current_type_params);
        let mut type_param_list = Vec::new();

        // First, collect type params from impl block's generic type (e.g., impl Box<T>)
        // Also build impl_type_params for the TirFunction
        let mut impl_type_params = Vec::new();
        if let ast::Type::Generic(generic) = impl_type {
            for (i, arg) in generic.args.iter().enumerate() {
                if let ast::Type::Named(named) = arg {
                    let name = &named.name;
                    if !self.current_type_params.contains_key(name) {
                        let type_id = self
                            .type_table
                            .borrow_mut()
                            .make_type_param(name.clone(), i as u32);
                        self.current_type_params
                            .insert(name.clone(), (i as u32, type_id));
                        // Store impl type param info for later monomorphization
                        impl_type_params.push(crate::tir::TirTypeParam {
                            name: name.clone(),
                            bounds: vec![],
                            index: i as u32,
                        });
                    }
                }
            }
        }

        // Then, collect method-level type params
        let offset = self.current_type_params.len();
        for (index, param) in func.type_params.iter().enumerate() {
            let idx = (offset + index) as u32;
            let type_id = self
                .type_table
                .borrow_mut()
                .make_type_param(param.name.clone(), idx);
            self.current_type_params
                .insert(param.name.clone(), (idx, type_id));
            type_param_list.push((param.name.clone(), type_id));
        }

        // Resolve return type
        let return_type = func
            .return_type
            .as_ref()
            .map(|t| self.resolve_type(t))
            .unwrap_or(TypeTable::UNIT);

        // Update the function_return_types with the resolved return type
        // (This replaces the potentially incorrect type from static resolution)
        // Use trait-mangled name for trait impls: StructName^TraitName::method_name
        let mangled_name = match trait_name {
            Some(t) => format!("{}^{}::{}", struct_name, t, func.name),
            None => format!("{}::{}", struct_name, func.name),
        };
        self.function_return_types
            .insert(mangled_name.clone(), return_type);

        let mut ctx = FunctionContext::new(return_type);

        // Resolve parameters (including &self)
        let mut params = Vec::new();
        for param in &func.params {
            let type_id = match param.self_kind {
                ast::SelfKind::Ref => {
                    // &self: wrap impl type in immutable reference
                    let inner_type = self.resolve_type(impl_type);
                    self.type_table.borrow_mut().make_ref(inner_type)
                }
                ast::SelfKind::MutRef => {
                    // &mut self: wrap impl type in mutable reference
                    let inner_type = self.resolve_type(impl_type);
                    self.type_table.borrow_mut().make_mut_ref(inner_type)
                }
                ast::SelfKind::None => {
                    // Regular parameter
                    self.resolve_type(&param.ty)
                }
            };
            let index = ctx.add_local(param.name.clone(), type_id, false);
            params.push(TirParam {
                name: param.name.clone(),
                type_id,
                local_index: index,
                span: param.span,
            });
        }

        // Resolve body
        let body = func.body.as_ref().map(|b| self.resolve_block(b, &mut ctx));

        // Restore previous type params scope
        self.current_type_params = old_type_params;

        // Store type parameters for generic methods (for call site substitution)
        if !func.type_params.is_empty() {
            self.generic_method_params
                .insert(mangled_name, type_param_list);
        }

        // Convert AST type params to TIR type params
        let type_params: Vec<crate::tir::TirTypeParam> = func
            .type_params
            .iter()
            .enumerate()
            .map(|(i, p)| crate::tir::TirTypeParam {
                name: p.name.clone(),
                bounds: p.bounds.clone(),
                index: i as u32,
            })
            .collect();

        Some(TirFunction {
            name: func.name.clone(), // Will be mangled by caller
            is_pub: func.is_pub,
            type_params,
            impl_type_params, // Type params from impl block (e.g., T from impl Counter<T>)
            monomorph_info: None, // Not from monomorphization
            params,
            return_type,
            effects: func.effects.clone(),
            body,
            span: func.span,
            local_count: ctx.next_local,
            local_types: ctx.local_types,
            address_taken_locals: ctx.address_taken_locals,
        })
    }

    /// Get the type name from a Type node
    fn get_type_name(&self, ty: &Type) -> String {
        match ty {
            Type::Named(named) => named.name.clone(),
            Type::Generic(generic) => generic.name.clone(),
            Type::Reference(inner) => self.get_type_name(inner),
            Type::MutReference(inner) => self.get_type_name(inner),
            _ => "Unknown".to_string(),
        }
    }

    /// Resolve a block
    fn resolve_block(&mut self, block: &Block, ctx: &mut FunctionContext) -> TirBlock {
        ctx.enter_scope();
        let stmts: Vec<TirStmt> = block
            .stmts
            .iter()
            .flat_map(|s| self.resolve_stmt(s, ctx))
            .collect();
        ctx.exit_scope();
        TirBlock::new(stmts, block.span)
    }

    /// Resolve a statement (may return multiple statements for desugared constructs)
    fn resolve_stmt(&mut self, stmt: &Stmt, ctx: &mut FunctionContext) -> Vec<TirStmt> {
        match stmt {
            Stmt::Let(let_stmt) => vec![self.resolve_let(let_stmt, ctx)],
            Stmt::Expr(expr_stmt) => vec![self.resolve_expr_stmt(expr_stmt, ctx)],
            Stmt::Return(ret_stmt) => vec![self.resolve_return(ret_stmt, ctx)],
            Stmt::If(if_stmt) => self.resolve_if_stmt(if_stmt, ctx),
            Stmt::While(while_stmt) => vec![self.resolve_while(while_stmt, ctx)],
            Stmt::For(for_stmt) => self.resolve_for(for_stmt, ctx),
            Stmt::ForOf(for_of_stmt) => vec![self.resolve_for_of(for_of_stmt, ctx)],
            Stmt::Loop(loop_stmt) => vec![self.resolve_loop(loop_stmt, ctx)],
            Stmt::Break(break_stmt) => vec![self.resolve_break(break_stmt)],
            Stmt::Continue(continue_stmt) => vec![self.resolve_continue(continue_stmt)],
            Stmt::Assert(_) => {
                // Assert statements are desugared in the desugar phase before resolution
                panic!("Assert should be desugared before resolving");
            }
            Stmt::LabeledBlock(labeled_block) => {
                vec![self.resolve_labeled_block(labeled_block, ctx)]
            }
        }
    }

    /// Resolve a labeled block statement
    fn resolve_labeled_block(
        &mut self,
        labeled_block: &ast::LabeledBlockStmt,
        ctx: &mut FunctionContext,
    ) -> TirStmt {
        // resolve_block already handles scope entry/exit
        let block = self.resolve_block(&labeled_block.block, ctx);

        TirStmt::new(
            TirStmtKind::LabeledBlock {
                label: labeled_block.label.clone(),
                block,
            },
            labeled_block.span,
        )
    }

    /// Resolve a let statement
    fn resolve_let(&mut self, let_stmt: &LetStmt, ctx: &mut FunctionContext) -> TirStmt {
        // Check for tuple literal to array coercion when type annotation is present
        let (value, type_id) = if let Some(annotated_type) = &let_stmt.ty {
            let target_type = self.resolve_type(annotated_type);

            // Special case: tuple literal with Array<T> or Tuple type annotation
            if let ast::Expr::TupleLiteral(tuple_lit) = &let_stmt.value {
                // let a: Array<i32> = [1, 2, 3]
                let element_type_opt = self.type_table.borrow().as_array(target_type);
                if let Some(element_type) = element_type_opt {
                    let elements: Vec<TirExpr> = tuple_lit
                        .elements
                        .iter()
                        .map(|elem| {
                            let resolved = self.resolve_expr(elem, ctx);
                            if resolved.type_id != element_type
                                && resolved.type_id != TypeTable::UNKNOWN
                            {
                                self.errors.push(TypeError::TypeMismatch {
                                    expected: self.type_table.borrow().type_name(element_type),
                                    found: self.type_table.borrow().type_name(resolved.type_id),
                                    span: elem.span(),
                                });
                            }
                            resolved
                        })
                        .collect();

                    let value = TirExpr::new(
                        TirExprKind::ArrayLiteral { elements },
                        target_type,
                        let_stmt.value.span(),
                    );
                    (value, target_type)
                } else {
                    let target_resolved = self.type_table.borrow().get(target_type).clone();
                    if let ResolvedType::Tuple(expected_elem_types) = target_resolved {
                        // let t: [i32, String] = [1, "hello"] - check element types
                        let expected_elem_types = expected_elem_types.clone();
                        let elements: Vec<TirExpr> = tuple_lit
                            .elements
                            .iter()
                            .enumerate()
                            .map(|(i, elem)| {
                                let resolved = self.resolve_expr(elem, ctx);
                                // Check if element type matches expected
                                if let Some(&expected_type) = expected_elem_types.get(i)
                                    && resolved.type_id != expected_type
                                    && resolved.type_id != TypeTable::UNKNOWN
                                {
                                    self.errors.push(TypeError::TypeMismatch {
                                        expected: self.type_table.borrow().type_name(expected_type),
                                        found: self.type_table.borrow().type_name(resolved.type_id),
                                        span: elem.span(),
                                    });
                                }
                                resolved
                            })
                            .collect();

                        // Also check length mismatch
                        if tuple_lit.elements.len() != expected_elem_types.len() {
                            self.errors.push(TypeError::TypeMismatch {
                                expected: format!(
                                    "tuple with {} elements",
                                    expected_elem_types.len()
                                ),
                                found: format!("tuple with {} elements", tuple_lit.elements.len()),
                                span: let_stmt.value.span(),
                            });
                        }

                        let value = TirExpr::new(
                            TirExprKind::TupleLiteral { elements },
                            target_type,
                            let_stmt.value.span(),
                        );
                        (value, target_type)
                    } else {
                        let value = self.resolve_expr(&let_stmt.value, ctx);
                        (value, target_type)
                    }
                }
            } else if let ast::Expr::StructLiteral(struct_lit) = &let_stmt.value {
                // Handle implicit struct literal: let p: Point = { x: 1, y: 2 }
                if struct_lit.name.is_none() {
                    // Check if target type is a struct
                    let target_resolved = self.type_table.borrow().get(target_type).clone();
                    if let ResolvedType::Struct {
                        name,
                        module_path: _,
                    } = target_resolved
                    {
                        let name = name.clone();
                        let struct_type = target_type;

                        let fields: Vec<TirStructField> = struct_lit
                            .fields
                            .iter()
                            .enumerate()
                            .map(|(index, field)| {
                                let value = self.resolve_expr(&field.value, ctx);
                                TirStructField {
                                    name: field.name.clone(),
                                    value,
                                    field_index: index as u32,
                                }
                            })
                            .collect();

                        let value = TirExpr::new(
                            TirExprKind::StructLiteral {
                                struct_type,
                                struct_name: name,
                                fields,
                            },
                            struct_type,
                            struct_lit.span,
                        );
                        (value, target_type)
                    } else {
                        // Target type is not a struct - error
                        self.errors.push(TypeError::TypeMismatch {
                            expected: self.type_table.borrow().type_name(target_type),
                            found: "implicit struct literal".into(),
                            span: struct_lit.span,
                        });
                        let value = self.resolve_expr(&let_stmt.value, ctx);
                        (value, target_type)
                    }
                } else {
                    // Named struct literal - resolve normally
                    let value = self.resolve_expr(&let_stmt.value, ctx);
                    (value, target_type)
                }
            } else {
                let value = self.resolve_expr(&let_stmt.value, ctx);
                (value, target_type)
            }
        } else {
            let value = self.resolve_expr(&let_stmt.value, ctx);
            (value.clone(), value.type_id)
        };

        let local_index = ctx.add_local(let_stmt.name.clone(), type_id, let_stmt.is_mut);

        TirStmt::new(
            TirStmtKind::Let {
                name: let_stmt.name.clone(),
                local_index,
                is_mut: let_stmt.is_mut,
                is_reactive: let_stmt.is_reactive,
                type_id,
                value,
            },
            let_stmt.span,
        )
    }

    /// Resolve an expression statement
    fn resolve_expr_stmt(&mut self, expr_stmt: &ExprStmt, ctx: &mut FunctionContext) -> TirStmt {
        let expr = self.resolve_expr(&expr_stmt.expr, ctx);
        TirStmt::new(TirStmtKind::Expr(expr), expr_stmt.span)
    }

    /// Resolve a return statement
    fn resolve_return(&mut self, ret_stmt: &ReturnStmt, ctx: &mut FunctionContext) -> TirStmt {
        let value = ret_stmt.value.as_ref().map(|expr| {
            // Check for tuple literal to array coercion based on function return type
            let element_type_opt = self.type_table.borrow().as_array(ctx.return_type);
            if let ast::Expr::TupleLiteral(tuple_lit) = expr
                && let Some(element_type) = element_type_opt
            {
                let elements: Vec<TirExpr> = tuple_lit
                    .elements
                    .iter()
                    .map(|elem| {
                        let resolved = self.resolve_expr(elem, ctx);
                        if resolved.type_id != element_type
                            && resolved.type_id != TypeTable::UNKNOWN
                        {
                            self.errors.push(TypeError::TypeMismatch {
                                expected: self.type_table.borrow().type_name(element_type),
                                found: self.type_table.borrow().type_name(resolved.type_id),
                                span: elem.span(),
                            });
                        }
                        resolved
                    })
                    .collect();

                return TirExpr::new(
                    TirExprKind::ArrayLiteral { elements },
                    ctx.return_type,
                    expr.span(),
                );
            }
            self.resolve_expr(expr, ctx)
        });
        TirStmt::new(TirStmtKind::Return { value }, ret_stmt.span)
    }

    /// Resolve an if statement
    /// Returns Vec<TirStmt> to handle if-let-init scoping: let binding + if statement
    fn resolve_if_stmt(&mut self, if_stmt: &IfStmt, ctx: &mut FunctionContext) -> Vec<TirStmt> {
        let mut result = Vec::new();

        // Handle optional init binding (scoped to this if statement)
        if if_stmt.init.is_some() {
            ctx.enter_scope();
        }

        if let Some(init) = &if_stmt.init {
            result.push(self.resolve_let(init, ctx));
        }

        match &if_stmt.condition {
            ast::IfCondition::Expr(expr) => {
                // Regular expression condition
                let condition = self.resolve_expr(expr, ctx);
                let then_block = self.resolve_block(&if_stmt.then_block, ctx);
                let else_block = if_stmt
                    .else_block
                    .as_ref()
                    .map(|b| self.resolve_block(b, ctx));

                result.push(TirStmt::new(
                    TirStmtKind::If {
                        condition,
                        then_block,
                        else_block,
                    },
                    if_stmt.span,
                ));
            }
            ast::IfCondition::Pattern { pattern, expr, .. } => {
                // Pattern match condition: if let Some(x) = expr { ... }
                let scrutinee = self.resolve_expr(expr, ctx);
                let scrutinee_type = scrutinee.type_id;

                // Enter scope for pattern bindings (they're only visible in then_block)
                ctx.enter_scope();

                // Resolve the pattern with type information from scrutinee
                let tir_pattern = self.resolve_if_pattern(pattern, scrutinee_type, ctx);

                let then_block = self.resolve_block(&if_stmt.then_block, ctx);

                // Exit pattern binding scope before resolving else block
                ctx.exit_scope();

                let else_block = if_stmt
                    .else_block
                    .as_ref()
                    .map(|b| self.resolve_block(b, ctx));

                result.push(TirStmt::new(
                    TirStmtKind::IfPattern {
                        scrutinee,
                        pattern: tir_pattern,
                        then_block,
                        else_block,
                    },
                    if_stmt.span,
                ));
            }
        }

        if if_stmt.init.is_some() {
            ctx.exit_scope();
        }

        result
    }

    /// Resolve a pattern in an if-pattern context with type information from the scrutinee
    fn resolve_if_pattern(
        &mut self,
        pattern: &Pattern,
        scrutinee_type: TypeId,
        ctx: &mut FunctionContext,
    ) -> TirPattern {
        match pattern {
            Pattern::Wildcard => TirPattern::Wildcard,
            Pattern::Ident(name) => {
                // The binding gets the scrutinee type (or inner type for Option patterns)
                let index = ctx.add_local(name.clone(), scrutinee_type, false);
                TirPattern::Binding {
                    name: name.clone(),
                    local_index: index,
                }
            }
            Pattern::Literal(lit) => {
                let tir_lit = match lit {
                    Literal::Int(i) => match Self::parse_int_literal(&i.repr) {
                        Ok(value) => TirLiteralPattern::Int(value),
                        Err(_) => TirLiteralPattern::Int(0),
                    },
                    Literal::Bool(b) => TirLiteralPattern::Bool(*b),
                    Literal::Char(c) => TirLiteralPattern::Char(*c),
                    Literal::String(s) => TirLiteralPattern::String(s.clone()),
                    Literal::Null => TirLiteralPattern::Null,
                    _ => TirLiteralPattern::Null,
                };
                TirPattern::Literal(tir_lit)
            }
            Pattern::Tuple(patterns) => {
                // For tuple patterns, extract element types
                let element_types = if let ResolvedType::Tuple(types) =
                    self.type_table.borrow().get(scrutinee_type).clone()
                {
                    types
                } else {
                    vec![TypeTable::UNKNOWN; patterns.len()]
                };

                let resolved: Vec<TirPattern> = patterns
                    .iter()
                    .zip(
                        element_types
                            .iter()
                            .chain(std::iter::repeat(&TypeTable::UNKNOWN)),
                    )
                    .map(|(p, &ty)| self.resolve_if_pattern(p, ty, ctx))
                    .collect();
                TirPattern::Tuple(resolved)
            }
            Pattern::Variant {
                variant_name,
                bindings,
                span,
            } => {
                // For Option patterns: Some(x) or None
                let inner_type = if let ResolvedType::Option(inner) =
                    self.type_table.borrow().get(scrutinee_type).clone()
                {
                    inner
                } else {
                    self.errors.push(TypeError::TypeMismatch {
                        expected: "Option type".to_string(),
                        found: format!("{:?}", self.type_table.borrow().get(scrutinee_type)),
                        span: *span,
                    });
                    TypeTable::UNKNOWN
                };

                // Resolve inner bindings with the inner type
                let resolved_bindings: Vec<TirPattern> = bindings
                    .iter()
                    .map(|p| self.resolve_if_pattern(p, inner_type, ctx))
                    .collect();

                TirPattern::Variant {
                    enum_type: scrutinee_type,
                    variant_name: variant_name.clone(),
                    bindings: resolved_bindings,
                }
            }
        }
    }

    /// Resolve a while statement
    fn resolve_while(&mut self, while_stmt: &WhileStmt, ctx: &mut FunctionContext) -> TirStmt {
        let condition = self.resolve_expr(&while_stmt.condition, ctx);
        let body = self.resolve_block(&while_stmt.body, ctx);

        TirStmt::new(TirStmtKind::While { condition, body }, while_stmt.span)
    }

    /// Resolve a for statement - generates init + For node wrapped in a scope
    /// The For node handles continue correctly (executes update before next iteration)
    /// The init variable is scoped to the for loop and not visible after it
    fn resolve_for(&mut self, for_stmt: &ForStmt, ctx: &mut FunctionContext) -> Vec<TirStmt> {
        // Enter scope for the for loop's init variable
        ctx.enter_scope();

        let mut result = Vec::new();

        // Add init statement if present (e.g., let i = 0)
        if let Some(init_stmt) = &for_stmt.init {
            result.extend(self.resolve_stmt(init_stmt, ctx));
        }

        // Resolve the body (note: resolve_block enters its own scope for body variables)
        let body = self.resolve_block(&for_stmt.body, ctx);

        // Resolve condition (None means infinite loop)
        let condition = for_stmt
            .condition
            .as_ref()
            .map(|c| self.resolve_expr(c, ctx));

        // Resolve update expression
        let update = for_stmt.update.as_ref().map(|u| self.resolve_expr(u, ctx));

        // Create For statement
        let for_tir = TirStmt::new(
            TirStmtKind::For {
                condition,
                body,
                update,
            },
            for_stmt.span,
        );
        result.push(for_tir);

        // Exit the for loop's scope
        ctx.exit_scope();

        result
    }

    /// Resolve a for-of statement: `for let item of array { ... }`
    fn resolve_for_of(&mut self, for_of_stmt: &ForOfStmt, ctx: &mut FunctionContext) -> TirStmt {
        // Resolve the iterable expression
        let iterable = self.resolve_expr(&for_of_stmt.iterable, ctx);
        let iterable_type = iterable.type_id;

        // Get the element type from the array type
        let element_type = if let Some(elem_type) = self.type_table.borrow().as_array(iterable_type)
        {
            elem_type
        } else {
            self.errors.push(TypeError::TypeMismatch {
                expected: "Array<T>".to_string(),
                found: self.type_table.borrow().type_name(iterable_type),
                span: for_of_stmt.iterable.span(),
            });
            TypeTable::UNKNOWN
        };

        // Enter a scope for the loop binding and body
        ctx.enter_scope();

        // Add the loop variable
        let binding_local = ctx.add_local(
            for_of_stmt.binding.clone(),
            element_type,
            for_of_stmt.is_mut,
        );

        // Resolve the body
        let body = self.resolve_block(&for_of_stmt.body, ctx);

        // Exit the scope
        ctx.exit_scope();

        TirStmt::new(
            TirStmtKind::ForOf {
                binding_local,
                binding_type: element_type,
                is_mut: for_of_stmt.is_mut,
                iterable,
                iterable_type,
                body,
            },
            for_of_stmt.span,
        )
    }

    /// Resolve a loop statement (infinite loop)
    fn resolve_loop(&mut self, loop_stmt: &LoopStmt, ctx: &mut FunctionContext) -> TirStmt {
        let body = self.resolve_block(&loop_stmt.body, ctx);
        TirStmt::new(TirStmtKind::Loop { body }, loop_stmt.span)
    }

    /// Resolve a break statement
    fn resolve_break(&mut self, break_stmt: &BreakStmt) -> TirStmt {
        TirStmt::new(TirStmtKind::Break, break_stmt.span)
    }

    /// Resolve a continue statement
    fn resolve_continue(&mut self, continue_stmt: &ContinueStmt) -> TirStmt {
        TirStmt::new(TirStmtKind::Continue, continue_stmt.span)
    }

    /// Resolve an expression
    fn resolve_expr(&mut self, expr: &Expr, ctx: &mut FunctionContext) -> TirExpr {
        match expr {
            Expr::Literal(lit) => self.resolve_literal(lit),
            Expr::Ident(ident) => self.resolve_ident(ident, ctx),
            Expr::Binary(binary) => self.resolve_binary(binary, ctx),
            Expr::Unary(unary) => self.resolve_unary(unary, ctx),
            Expr::Assign(assign) => self.resolve_assign(assign, ctx),
            Expr::Call(call) => self.resolve_call(call, ctx),
            Expr::MethodCall(method_call) => self.resolve_method_call(method_call, ctx),
            Expr::StaticMethodCall(static_call) => {
                self.resolve_static_method_call(static_call, ctx)
            }
            Expr::FieldAccess(field_access) => self.resolve_field_access(field_access, ctx),
            Expr::Index(index) => self.resolve_index(index, ctx),
            Expr::Block(block) => {
                let tir_block = self.resolve_block(block, ctx);
                // Block expression type is the last expression's type or Unit
                let type_id = tir_block
                    .stmts
                    .last()
                    .and_then(|s| match &s.kind {
                        TirStmtKind::Expr(e) => Some(e.type_id),
                        _ => None,
                    })
                    .unwrap_or(TypeTable::UNIT);
                TirExpr::new(TirExprKind::Block(tir_block), type_id, block.span)
            }
            Expr::If(if_expr) => self.resolve_if_expr(if_expr, ctx),
            Expr::Match(match_expr) => self.resolve_match_expr(match_expr, ctx),
            Expr::Closure(closure) => self.resolve_closure(closure, ctx),
            Expr::TemplateString(template) => self.resolve_template_string(template, ctx),
            Expr::Cast(cast) => self.resolve_cast(cast, ctx),
            Expr::StructLiteral(struct_lit) => self.resolve_struct_literal(struct_lit, ctx),
            Expr::CompoundAssign(compound) => self.resolve_compound_assign(compound, ctx),
            Expr::ComparisonChain(chain) => self.resolve_comparison_chain(chain, ctx),
            Expr::TupleLiteral(tuple_lit) => self.resolve_tuple_literal(tuple_lit, ctx),
        }
    }

    /// Parse an integer literal string into a u64 value
    fn parse_int_literal(repr: &str) -> Result<u64, String> {
        // Remove underscores for parsing
        let clean: String = repr.chars().filter(|&c| c != '_').collect();

        if clean.starts_with("0x") || clean.starts_with("0X") {
            u64::from_str_radix(&clean[2..], 16).map_err(|_| format!("invalid hex literal: {repr}"))
        } else if clean.starts_with("0b") || clean.starts_with("0B") {
            u64::from_str_radix(&clean[2..], 2)
                .map_err(|_| format!("invalid binary literal: {repr}"))
        } else if clean.starts_with("0o") || clean.starts_with("0O") {
            u64::from_str_radix(&clean[2..], 8)
                .map_err(|_| format!("invalid octal literal: {repr}"))
        } else {
            clean
                .parse()
                .map_err(|_| format!("invalid integer literal: {repr}"))
        }
    }

    /// Parse a float literal string into an f64 value
    fn parse_float_literal(repr: &str) -> Result<f64, String> {
        // Remove underscores for parsing
        let clean: String = repr.chars().filter(|&c| c != '_').collect();
        clean
            .parse()
            .map_err(|_| format!("invalid float literal: {repr}"))
    }

    /// Resolve a literal expression
    fn resolve_literal(&mut self, lit: &ast::LiteralExpr) -> TirExpr {
        let (kind, type_id) = match &lit.value {
            Literal::Int(int_lit) => {
                match Self::parse_int_literal(&int_lit.repr) {
                    Ok(value) => (
                        TirExprKind::IntLiteral {
                            value,
                            repr: int_lit.repr.clone(),
                        },
                        TypeTable::I32,
                    ),
                    Err(message) => {
                        self.errors.push(TypeError::InvalidLiteral {
                            message,
                            span: lit.span,
                        });
                        // Return 0 as fallback to continue resolution
                        (
                            TirExprKind::IntLiteral {
                                value: 0,
                                repr: int_lit.repr.clone(),
                            },
                            TypeTable::I32,
                        )
                    }
                }
            }
            Literal::Float(float_lit) => {
                match Self::parse_float_literal(&float_lit.repr) {
                    Ok(value) => (
                        TirExprKind::FloatLiteral {
                            value,
                            repr: float_lit.repr.clone(),
                        },
                        TypeTable::F64,
                    ),
                    Err(message) => {
                        self.errors.push(TypeError::InvalidLiteral {
                            message,
                            span: lit.span,
                        });
                        // Return 0.0 as fallback to continue resolution
                        (
                            TirExprKind::FloatLiteral {
                                value: 0.0,
                                repr: float_lit.repr.clone(),
                            },
                            TypeTable::F64,
                        )
                    }
                }
            }
            Literal::Bool(b) => (TirExprKind::BoolLiteral(*b), TypeTable::BOOL),
            Literal::Char(c) => (TirExprKind::CharLiteral(*c), TypeTable::CHAR),
            Literal::String(s) => {
                let string_type = self.get_string_struct_type();
                (TirExprKind::StringLiteral(s.clone()), string_type)
            }
            Literal::Null => {
                // Null is Option<T> where T is unknown
                let option_unknown = self.type_table.borrow_mut().make_option(TypeTable::UNKNOWN);
                (TirExprKind::Null, option_unknown)
            }
            Literal::Unit => (TirExprKind::Unit, TypeTable::UNIT),
        };
        TirExpr::new(kind, type_id, lit.span)
    }

    /// Resolve an identifier expression
    fn resolve_ident(&mut self, ident: &ast::IdentExpr, ctx: &mut FunctionContext) -> TirExpr {
        // Check local variables, including captures from outer scope
        if let Some(var_ref) = ctx.lookup_or_capture(&ident.name) {
            match var_ref {
                VarRef::Local { index, type_id } => {
                    return TirExpr::new(
                        TirExprKind::Local {
                            index,
                            name: ident.name.clone(),
                        },
                        type_id,
                        ident.span,
                    );
                }
                VarRef::Capture { index, type_id } => {
                    return TirExpr::new(
                        TirExprKind::Capture {
                            index,
                            name: ident.name.clone(),
                        },
                        type_id,
                        ident.span,
                    );
                }
            }
        }

        // Check for qualified variant case names like Color::Red (without parentheses)
        if let Some(pos) = ident.name.find("::") {
            let prefix = &ident.name[..pos];
            let suffix = &ident.name[pos + 2..];

            if let Some((module_path, _, cases)) = self.variant_cases.get(prefix) {
                // Find the case by name
                if let Some((case_index, (case_name, field_types))) =
                    cases.iter().enumerate().find(|(_, (n, _))| n == suffix)
                {
                    // Unit variant - must have no fields
                    if !field_types.is_empty() {
                        self.errors.push(TypeError::ArgumentCountMismatch {
                            expected: field_types.len(),
                            found: 0,
                            span: ident.span,
                        });
                        return TirExpr::new(TirExprKind::Unit, TypeTable::ERROR, ident.span);
                    }

                    // Create variant type
                    let variant_type = self
                        .type_table
                        .borrow_mut()
                        .make_variant(prefix.to_string(), module_path.clone());

                    return TirExpr::new(
                        TirExprKind::VariantConstruct {
                            variant_type,
                            case_index: case_index as u32,
                            case_name: case_name.clone(),
                            fields: vec![],
                        },
                        variant_type,
                        ident.span,
                    );
                }
            }
        }

        // Otherwise it's a global reference (function, constant, etc.)
        // For now, return Unknown type - will be resolved by looking up in symbol table
        TirExpr::new(
            TirExprKind::Global {
                module_path: Vec::new(),
                name: ident.name.clone(),
            },
            TypeTable::UNKNOWN,
            ident.span,
        )
    }

    /// Resolve a binary expression
    fn resolve_binary(&mut self, binary: &ast::BinaryExpr, ctx: &mut FunctionContext) -> TirExpr {
        let left = self.resolve_expr(&binary.left, ctx);
        let right = self.resolve_expr(&binary.right, ctx);

        // Special case: String equality comparison
        // Desugar `a == b` to `string_eq(&a, &b)` and `a != b` to `!string_eq(&a, &b)`
        let left_type = self.type_table.borrow().get(left.type_id).clone();
        let is_string = matches!(left_type, ResolvedType::String)
            || matches!(left_type, ResolvedType::Struct { ref name, .. } if name == "String");
        if is_string && matches!(binary.op, BinaryOp::Eq | BinaryOp::NotEq) {
            // Create reference types for the arguments
            let left_ref_type = self
                .type_table
                .borrow_mut()
                .intern(ResolvedType::Ref(left.type_id));
            let right_ref_type = self
                .type_table
                .borrow_mut()
                .intern(ResolvedType::Ref(right.type_id));

            // Wrap left and right with Ref expressions
            let left_ref = TirExpr::new(
                TirExprKind::Unary {
                    op: TirUnaryOp::Ref,
                    expr: Box::new(left),
                },
                left_ref_type,
                binary.span,
            );
            let right_ref = TirExpr::new(
                TirExprKind::Unary {
                    op: TirUnaryOp::Ref,
                    expr: Box::new(right),
                },
                right_ref_type,
                binary.span,
            );

            let call_expr = TirExpr::new(
                TirExprKind::Call {
                    func: FunctionRef::External {
                        module_path: vec!["core".to_string(), "internal".to_string()],
                        name: "string_eq".to_string(),
                        monomorph_info: None,
                    },
                    type_args: vec![],
                    args: vec![left_ref, right_ref],
                },
                TypeTable::BOOL,
                binary.span,
            );

            // For NotEq, negate the result
            if binary.op == BinaryOp::NotEq {
                return TirExpr::new(
                    TirExprKind::Unary {
                        op: TirUnaryOp::Not,
                        expr: Box::new(call_expr),
                    },
                    TypeTable::BOOL,
                    binary.span,
                );
            }

            return call_expr;
        }

        let op = convert_binary_op(binary.op);

        // Determine result type based on operator
        let type_id = match binary.op {
            BinaryOp::Eq
            | BinaryOp::NotEq
            | BinaryOp::Lt
            | BinaryOp::LtEq
            | BinaryOp::Gt
            | BinaryOp::GtEq
            | BinaryOp::And
            | BinaryOp::Or => TypeTable::BOOL,
            _ => left.type_id, // Arithmetic ops preserve the type
        };

        TirExpr::new(
            TirExprKind::Binary {
                left: Box::new(left),
                op,
                right: Box::new(right),
            },
            type_id,
            binary.span,
        )
    }

    /// Resolve a unary expression
    fn resolve_unary(&mut self, unary: &ast::UnaryExpr, ctx: &mut FunctionContext) -> TirExpr {
        let expr = self.resolve_expr(&unary.expr, ctx);
        let op = convert_unary_op(unary.op);

        // Track address-taken locals for &x and &mut x
        if matches!(unary.op, UnaryOp::Ref | UnaryOp::MutRef)
            && let TirExprKind::Local { index, .. } = &expr.kind
        {
            ctx.address_taken_locals.insert(*index);
        }

        // Check that &mut is only applied to mutable locals
        if unary.op == UnaryOp::MutRef
            && let TirExprKind::Local { name, .. } = &expr.kind
            && let Some(local) = ctx.lookup(name)
            && !local.is_mut
        {
            self.errors.push(TypeError::TypeMismatch {
                expected: "mutable variable".to_string(),
                found: format!("immutable variable '{}'", name),
                span: unary.span,
            });
        }

        let type_id = match unary.op {
            UnaryOp::Not => TypeTable::BOOL,
            UnaryOp::Ref => self.type_table.borrow_mut().make_ref(expr.type_id),
            UnaryOp::MutRef => self.type_table.borrow_mut().make_mut_ref(expr.type_id),
            UnaryOp::Deref => {
                // Dereference returns the inner type
                if let ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) =
                    self.type_table.borrow().get(expr.type_id)
                {
                    *inner
                } else {
                    // Cannot dereference non-reference type
                    self.errors.push(TypeError::TypeMismatch {
                        expected: "reference type".to_string(),
                        found: self.type_table.borrow().type_name(expr.type_id),
                        span: unary.span,
                    });
                    TypeTable::ERROR
                }
            }
            _ => expr.type_id,
        };

        TirExpr::new(
            TirExprKind::Unary {
                op,
                expr: Box::new(expr),
            },
            type_id,
            unary.span,
        )
    }

    /// Resolve an assignment expression
    fn resolve_assign(&mut self, assign: &ast::AssignExpr, ctx: &mut FunctionContext) -> TirExpr {
        let target = self.resolve_expr(&assign.target, ctx);
        let value = self.resolve_expr(&assign.value, ctx);

        // Validate that the target is a valid l-value
        let is_valid_lvalue = match &target.kind {
            TirExprKind::Local { .. } => true,
            TirExprKind::FieldAccess { .. } => true,
            TirExprKind::Index { .. } => true,
            // Dereference is a valid l-value: *ref = value
            TirExprKind::Unary {
                op: TirUnaryOp::Deref,
                ..
            } => true,
            _ => false,
        };

        if !is_valid_lvalue {
            // Report error for invalid assignment target
            self.errors.push(TypeError::CannotAssign {
                message: "expression is not assignable".to_string(),
                span: assign.target.span(),
            });
            return TirExpr::new(TirExprKind::Unit, TypeTable::ERROR, assign.span);
        }

        TirExpr::new(
            TirExprKind::Assign {
                target: Box::new(target),
                value: Box::new(value.clone()),
            },
            value.type_id,
            assign.span,
        )
    }

    /// Resolve a compound assignment (already desugared, but handle anyway)
    fn resolve_compound_assign(
        &mut self,
        compound: &ast::CompoundAssignExpr,
        ctx: &mut FunctionContext,
    ) -> TirExpr {
        // This should have been desugared, but handle it anyway
        let target = self.resolve_expr(&compound.target, ctx);
        let value = self.resolve_expr(&compound.value, ctx);

        let op = match compound.op {
            ast::CompoundAssignOp::Add => TirBinaryOp::Add,
            ast::CompoundAssignOp::Sub => TirBinaryOp::Sub,
            ast::CompoundAssignOp::Mul => TirBinaryOp::Mul,
            ast::CompoundAssignOp::Div => TirBinaryOp::Div,
            ast::CompoundAssignOp::Mod => TirBinaryOp::Mod,
        };

        // target = target op value
        let binary = TirExpr::new(
            TirExprKind::Binary {
                left: Box::new(target.clone()),
                op,
                right: Box::new(value),
            },
            target.type_id,
            compound.span,
        );

        TirExpr::new(
            TirExprKind::Assign {
                target: Box::new(target),
                value: Box::new(binary),
            },
            TypeTable::UNIT,
            compound.span,
        )
    }

    /// Resolve a comparison chain (already desugared, but handle anyway)
    fn resolve_comparison_chain(
        &mut self,
        chain: &ast::ComparisonChainExpr,
        ctx: &mut FunctionContext,
    ) -> TirExpr {
        // This should have been desugared to binary && chain
        // Just resolve the first expression for now
        self.resolve_expr(&chain.first, ctx)
    }

    /// Resolve a call expression
    fn resolve_call(&mut self, call: &ast::CallExpr, ctx: &mut FunctionContext) -> TirExpr {
        // Check if this is a closure call (calling a local variable with function type)
        if let Expr::Ident(ident) = &call.callee {
            // No :: means it could be a local variable
            if !ident.name.contains("::")
                && let Some(local) = ctx.lookup(&ident.name)
            {
                // Check if the local has a function type
                let local_type = self.type_table.borrow().get(local.type_id).clone();
                if let ResolvedType::Function {
                    params: fn_params,
                    return_type,
                    ..
                } = local_type
                {
                    // This is a closure call!
                    let local_index = local.index;
                    let local_type_id = local.type_id;
                    let fn_return_type = return_type;
                    // Clone fn_params to avoid borrow conflict
                    let fn_params = fn_params.clone();

                    // Resolve arguments with coercion awareness based on closure param types
                    let args: Vec<TirExpr> = call
                        .args
                        .iter()
                        .enumerate()
                        .map(|(i, arg)| {
                            let expected_type = fn_params.get(i).copied();
                            self.resolve_expr_with_expected_type(arg, ctx, expected_type)
                        })
                        .collect();

                    // Create closure expression (Local reference)
                    let closure_expr = TirExpr::new(
                        TirExprKind::Local {
                            index: local_index,
                            name: ident.name.clone(),
                        },
                        local_type_id,
                        ident.span,
                    );

                    return TirExpr::new(
                        TirExprKind::IndirectCall {
                            callee: Box::new(closure_expr),
                            args,
                        },
                        fn_return_type,
                        call.span,
                    );
                }
            }
        }

        // First, determine expected parameter types to handle coercion
        let param_types = self.lookup_function_param_types(&call.callee);

        // Resolve arguments with coercion awareness
        let args: Vec<TirExpr> = call
            .args
            .iter()
            .enumerate()
            .map(|(i, arg)| {
                let expected_type = param_types.get(i).copied();
                self.resolve_expr_with_expected_type(arg, ctx, expected_type)
            })
            .collect();

        // Get function name from callee
        let (module_path, func_name, is_known) = match &call.callee {
            Expr::Ident(ident) => {
                // Check for qualified name with :: (e.g., "Stdout::write_via_stream")
                // Parser creates a single ident for Effect::operation syntax
                if let Some(pos) = ident.name.find("::") {
                    let prefix = &ident.name[..pos];
                    let suffix = &ident.name[pos + 2..];

                    // Builtin functions: resolve through core:builtin module
                    if prefix == "builtin" {
                        (
                            vec!["core".to_string(), "builtin".to_string()],
                            suffix.to_string(),
                            true,
                        )
                    }
                    // Check if this is a static method call (Type::method)
                    // Static methods are registered with mangled names "Type::method"
                    else if self.is_static_method(prefix, suffix) {
                        // Return as a static method call - will be converted to StaticCall below
                        let mangled_name = format!("{}::{}", prefix, suffix);
                        return self.resolve_static_method_call_from_qualified(
                            prefix,
                            suffix,
                            &mangled_name,
                            &args,
                            call.span,
                            ctx,
                        );
                    }
                    // Check if this is a variant case construction (Color::Red)
                    else if let Some((module_path, _, cases)) = self.variant_cases.get(prefix) {
                        // Find the case by name
                        if let Some((case_index, (case_name, field_types))) =
                            cases.iter().enumerate().find(|(_, (n, _))| n == suffix)
                        {
                            // Validate argument count
                            if args.len() != field_types.len() {
                                self.errors.push(TypeError::ArgumentCountMismatch {
                                    expected: field_types.len(),
                                    found: args.len(),
                                    span: call.span,
                                });
                                return TirExpr::new(
                                    TirExprKind::Unit,
                                    TypeTable::ERROR,
                                    call.span,
                                );
                            }

                            // Create variant type
                            let variant_type = self
                                .type_table
                                .borrow_mut()
                                .make_variant(prefix.to_string(), module_path.clone());

                            return TirExpr::new(
                                TirExprKind::VariantConstruct {
                                    variant_type,
                                    case_index: case_index as u32,
                                    case_name: case_name.clone(),
                                    fields: args,
                                },
                                variant_type,
                                call.span,
                            );
                        } else {
                            // Unknown case name
                            self.errors.push(TypeError::UnknownFunction {
                                name: format!("{}::{}", prefix, suffix),
                                span: call.span,
                            });
                            return TirExpr::new(TirExprKind::Unit, TypeTable::ERROR, call.span);
                        }
                    }
                    // Effect operations and other qualified calls - always allowed
                    // (validated by effect system/codegen)
                    else {
                        (vec![prefix.to_string()], suffix.to_string(), true)
                    }
                }
                // Check if it's a local function (defined in this module) or
                // a built-in type constructor (Ok, Err, Some, None)
                else if self.function_return_types.contains_key(&ident.name)
                    || matches!(ident.name.as_str(), "Ok" | "Err" | "Some" | "None")
                {
                    (Vec::new(), ident.name.clone(), true)
                }
                // Check for prelude functions (panic, unreachable)
                // These are actual functions in core::prelude
                else if matches!(ident.name.as_str(), "panic" | "unreachable") {
                    (
                        vec!["core".to_string(), "prelude".to_string()],
                        ident.name.clone(),
                        true,
                    )
                }
                // Check if this is an imported function (per-module imports)
                else if self.imported_functions.contains(&ident.name) {
                    // Get module path from symbol table for codegen
                    if let Some(symbol) = self.symbols.lookup(&ident.name) {
                        (symbol.module_path.clone(), symbol.name.clone(), true)
                    } else {
                        // Imported but not in symbols - shouldn't happen but allow
                        (Vec::new(), ident.name.clone(), true)
                    }
                } else {
                    // Unknown function - will report error
                    (Vec::new(), ident.name.clone(), false)
                }
            }
            Expr::FieldAccess(field_access) => {
                // e.g., Stdout.write (unlikely but possible)
                // These are always considered known - validated elsewhere
                if let Expr::Ident(ident) = &field_access.expr {
                    (vec![ident.name.clone()], field_access.field.clone(), true)
                } else {
                    (Vec::new(), String::from("unknown"), false)
                }
            }
            _ => (Vec::new(), String::from("unknown"), false),
        };

        // Report error for unknown functions
        if !is_known {
            self.errors.push(TypeError::UnknownFunction {
                name: func_name.clone(),
                span: call.span,
            });
        }

        // Resolve explicit type arguments
        let type_args: Vec<TypeId> = call
            .type_args
            .iter()
            .map(|ty| self.resolve_type(ty))
            .collect();

        // Look up function return type
        let mut return_type = self.lookup_function_return_type(&module_path, &func_name);

        // If we have explicit type args, substitute type parameters in the return type
        if !type_args.is_empty() {
            return_type = self.substitute_type_params(return_type, &type_args);
        }

        TirExpr::new(
            TirExprKind::Call {
                func: FunctionRef::External {
                    module_path,
                    name: func_name,
                    monomorph_info: None,
                },
                type_args,
                args,
            },
            return_type,
            call.span,
        )
    }

    /// Look up the return type of a function
    fn lookup_function_return_type(&mut self, module_path: &[String], func_name: &str) -> TypeId {
        // Handle builtin functions
        // Normal resolution: module_path == ["core", "builtin"]
        if module_path.len() == 2 && module_path[0] == "core" && module_path[1] == "builtin" {
            return self.get_builtin_return_type(func_name);
        }
        // Legacy: builtin::name pattern
        if let Some(builtin_name) = func_name.strip_prefix("builtin::") {
            return self.get_builtin_return_type(builtin_name);
        }

        // Handle WASI effect operations (e.g., Environment::get_arguments)
        if module_path.len() == 1
            && let Some(return_type) = self.get_wasi_effect_return_type(&module_path[0], func_name)
        {
            return return_type;
        }

        // First, try local functions (no module path)
        if module_path.is_empty()
            && let Some(&return_type) = self.function_return_types.get(func_name)
        {
            return return_type;
        }

        // Try looking up in loaded modules
        if !module_path.is_empty() {
            // Clone the return type AST to avoid borrow issues
            let return_type_ast = self.loaded_modules.get(module_path).and_then(|module| {
                module.items.iter().find_map(|item| {
                    if let Item::Function(func) = item
                        && func.name == func_name
                    {
                        func.return_type.clone()
                    } else {
                        None
                    }
                })
            });

            if let Some(ty) = return_type_ast {
                return self.resolve_type(&ty);
            }
        }

        // Default to UNIT for unknown functions (they might be external/builtin)
        TypeTable::UNIT
    }

    /// Get the return type of a WASI effect operation
    fn get_wasi_effect_return_type(&mut self, effect: &str, operation: &str) -> Option<TypeId> {
        let string_type = self.get_string_struct_type();
        match (effect, operation) {
            // Environment effect operations
            ("Environment", "get_arguments") => {
                Some(self.type_table.borrow_mut().make_array(string_type))
            }
            ("Environment", "get_environment") => {
                // Returns Array<[String, String]> - array of key-value tuple pairs
                let tuple_type = self
                    .type_table
                    .borrow_mut()
                    .intern(ResolvedType::Tuple(vec![string_type, string_type]));
                Some(self.type_table.borrow_mut().make_array(tuple_type))
            }
            ("Environment", "get_initial_cwd") => Some(
                self.type_table
                    .borrow_mut()
                    .intern(ResolvedType::Option(string_type)),
            ),
            _ => None,
        }
    }

    /// Get the String struct type (from prelude)
    fn get_string_struct_type(&mut self) -> TypeId {
        self.type_table.borrow_mut().make_struct(
            "String".to_string(),
            vec!["core".to_string(), "prelude".to_string()],
        )
    }

    /// Get the return type of a builtin function
    fn get_builtin_return_type(&mut self, name: &str) -> TypeId {
        match name {
            // Generic array operations - return types with TypeParam for substitution
            // These are called with type arguments: array_new::<T>() -> builtin::array<T>
            "array_new" => {
                // Returns builtin::array<T> where T is the first type param
                let type_param = self
                    .type_table
                    .borrow_mut()
                    .intern(ResolvedType::TypeParam {
                        index: 0,
                        name: "T".to_string(),
                    });
                self.type_table
                    .borrow_mut()
                    .intern(ResolvedType::BuiltinArray(type_param))
            }
            "array_get" => {
                // Returns T where T is the first type param
                self.type_table
                    .borrow_mut()
                    .intern(ResolvedType::TypeParam {
                        index: 0,
                        name: "T".to_string(),
                    })
            }
            "array_set" | "array_copy" => TypeTable::UNIT,

            // Non-generic array operations
            "array_len" => TypeTable::I32,
            "array_get_u8" => TypeTable::I32, // Returns u8 as i32
            "array_set_u8" => TypeTable::UNIT,
            "string_new" => self.get_string_struct_type(),
            "array_new_string" => {
                // Returns builtin::array<String>, not Array<String>
                let string_type = self.get_string_struct_type();
                self.type_table
                    .borrow_mut()
                    .intern(ResolvedType::BuiltinArray(string_type))
            }
            "array_get_string" => self.get_string_struct_type(),
            "array_set_string" => TypeTable::UNIT,

            // Memory operations
            "realloc" => TypeTable::I32, // Returns pointer (i32)
            "memory_load8_u" => TypeTable::I32,
            "memory_load32" => TypeTable::I32,
            "memory_store8" => TypeTable::UNIT,

            // Float-to-string (returns length)
            "f64_to_buffer" | "f32_to_buffer" => TypeTable::I32,

            // Bitwise operations
            "i32_and" | "i32_or" | "i32_xor" | "i32_shl" | "i32_shr_u" | "i32_shr_s" => {
                TypeTable::I32
            }
            "i32_eqz" => TypeTable::BOOL,

            // Control flow / hints
            "likely" | "unlikely" => TypeTable::BOOL,
            "unreachable" | "effect_wait" => TypeTable::NEVER,

            // Stream builtins for IO
            "call_indirect_stdout_write_via_stream" | "call_indirect_stderr_write_via_stream" => {
                TypeTable::UNIT
            }

            // WASI stream operations
            "stream_new" => TypeTable::I64, // Returns i64 (two i32s packed)
            "stream_write" => TypeTable::I32, // Returns result code
            "stream_drop_writable" | "stream_drop_readable" => TypeTable::UNIT,

            // Unknown builtin - default to UNIT
            _ => TypeTable::UNIT,
        }
    }

    /// Look up function parameter types from callee expression
    fn lookup_function_param_types(&mut self, callee: &Expr) -> Vec<TypeId> {
        match callee {
            Expr::Ident(ident) => {
                // Check for qualified name (Effect::operation)
                if ident.name.contains("::") {
                    return Vec::new(); // Effect operations handled separately
                }

                // Check if it's a local function (defined in this module)
                if self.function_return_types.contains_key(&ident.name) {
                    // Clone params to avoid borrow issues
                    let params: Option<Vec<_>> =
                        self.current_module_items.iter().find_map(|item| {
                            if let Item::Function(func) = item
                                && func.name == ident.name
                            {
                                return Some(func.params.clone());
                            }
                            None
                        });

                    if let Some(params) = params {
                        return params.iter().map(|p| self.resolve_type(&p.ty)).collect();
                    }
                }

                // Check imported functions
                if let Some(symbol) = self.symbols.lookup(&ident.name)
                    && let Some(module) = self.loaded_modules.get(&symbol.module_path)
                {
                    // Clone params to avoid borrow issues
                    let params: Option<Vec<_>> = module.items.iter().find_map(|item| {
                        if let Item::Function(func) = item
                            && func.name == symbol.name
                        {
                            return Some(func.params.clone());
                        }
                        None
                    });

                    if let Some(params) = params {
                        return params.iter().map(|p| self.resolve_type(&p.ty)).collect();
                    }
                }

                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// Resolve an expression with an expected type for coercion
    fn resolve_expr_with_expected_type(
        &mut self,
        expr: &Expr,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
    ) -> TirExpr {
        // Handle tuple literal to array coercion
        let element_type_opt = expected_type.and_then(|t| self.type_table.borrow().as_array(t));
        if let Some(target_type) = expected_type
            && let Expr::TupleLiteral(tuple_lit) = expr
            && let Some(element_type) = element_type_opt
        {
            let elements: Vec<TirExpr> = tuple_lit
                .elements
                .iter()
                .map(|elem| {
                    let resolved = self.resolve_expr(elem, ctx);
                    if resolved.type_id != element_type && resolved.type_id != TypeTable::UNKNOWN {
                        self.errors.push(TypeError::TypeMismatch {
                            expected: self.type_table.borrow().type_name(element_type),
                            found: self.type_table.borrow().type_name(resolved.type_id),
                            span: elem.span(),
                        });
                    }
                    resolved
                })
                .collect();

            return TirExpr::new(
                TirExprKind::ArrayLiteral { elements },
                target_type,
                expr.span(),
            );
        }

        // Normal expression resolution
        self.resolve_expr(expr, ctx)
    }

    /// Resolve a type without registering new types
    /// This is used for lookups where we need immutable access. It only handles
    /// primitive types and type aliases. For generic types, use resolve_type instead.
    fn resolve_type_no_register(&self, ty: &Type) -> TypeId {
        match ty {
            Type::Named(named) => match named.name.as_str() {
                "i8" => TypeTable::I8,
                "i16" => TypeTable::I16,
                "i32" => TypeTable::I32,
                "i64" => TypeTable::I64,
                "i128" => TypeTable::I128,
                "u8" => TypeTable::U8,
                "u16" => TypeTable::U16,
                "u32" => TypeTable::U32,
                "u64" => TypeTable::U64,
                "u128" => TypeTable::U128,
                "f32" => TypeTable::F32,
                "f64" => TypeTable::F64,
                "bool" => TypeTable::BOOL,
                "char" => TypeTable::CHAR,
                "!" => TypeTable::NEVER,
                "()" => TypeTable::UNIT,
                _ => {
                    // Check type aliases (e.g., Instant = u64, Duration = u64)
                    if let Some(&type_id) = self.type_aliases.get(&named.name) {
                        type_id
                    } else {
                        TypeTable::UNKNOWN
                    }
                }
            },
            _ => TypeTable::UNKNOWN,
        }
    }

    /// Resolve a method call
    fn resolve_method_call(
        &mut self,
        method_call: &ast::MethodCallExpr,
        ctx: &mut FunctionContext,
    ) -> TirExpr {
        let mut receiver = self.resolve_expr(&method_call.receiver, ctx);
        let args: Vec<TirExpr> = method_call
            .args
            .iter()
            .map(|a| self.resolve_expr(a, ctx))
            .collect();

        // Resolve explicit type arguments (method-level type args)
        let type_args: Vec<TypeId> = method_call
            .type_args
            .iter()
            .map(|ty| self.resolve_type(ty))
            .collect();

        // Get the base (non-ref) type for method lookup and struct name extraction
        let base_type_id = self.get_base_type(receiver.type_id);

        // Get struct name from base type
        let (struct_name, module_path) = match self.type_table.borrow().get(base_type_id) {
            ResolvedType::Struct { name, module_path } => (name.clone(), module_path.clone()),
            ResolvedType::GenericInstance {
                name, module_path, ..
            } => (name.clone(), module_path.clone()),
            _ => (self.mangle_type_name(base_type_id), vec![]),
        };

        // Look up method info based on receiver type
        // First try inherent method, then trait methods
        let mut method_info = self.lookup_method_info(receiver.type_id, &method_call.method);
        let mut trait_name: Option<String> = None;

        // If inherent method not found, try trait methods
        if method_info.is_none()
            && let Some((found_trait, info)) =
                self.find_trait_method_for_type(&struct_name, &method_call.method, &module_path)
        {
            trait_name = Some(found_trait);
            method_info = Some(info);
        }

        // Get method info (with default fallback)
        let MethodInfo {
            mut return_type,
            self_kind,
        } = method_info.unwrap_or(MethodInfo {
            return_type: TypeTable::UNKNOWN,
            self_kind: ast::SelfKind::Ref, // Default to &self
        });

        // Adjust receiver based on what the method expects (self_kind)
        receiver = self.adjust_receiver_for_self_kind(receiver, self_kind, method_call.span);

        // Build unified substitution context for double generics
        // Type param indices are assigned as follows:
        // - Impl type params (from struct): 0, 1, 2, ...
        // - Method type params: offset, offset+1, ... (where offset = impl_type_params.len())
        let mut subst_ctx = SubstitutionContext::new();
        let mut impl_offset = 0u32;

        // First, add impl-level type args from receiver's generic type (use base type)
        if let ResolvedType::GenericInstance {
            type_args: receiver_type_args,
            ..
        } = self.type_table.borrow().get(base_type_id).clone()
            && !receiver_type_args.is_empty()
        {
            impl_offset = receiver_type_args.len() as u32;
            subst_ctx = subst_ctx.with_impl_args(&receiver_type_args);
        }

        // Then add method-level type args with the correct offset
        if !type_args.is_empty() {
            subst_ctx = subst_ctx.with_method_args(&type_args, impl_offset);
        }

        // Apply unified substitution
        if !subst_ctx.is_empty() {
            return_type = subst_ctx.substitute(return_type, &mut self.type_table.borrow_mut());
        }

        // Get struct name and monomorph info from base type for mangled method name
        let (receiver_struct_name, base_struct_name, receiver_type_args) =
            match self.type_table.borrow().get(base_type_id).clone() {
                ResolvedType::GenericInstance {
                    name, type_args, ..
                } => {
                    let type_arg_names: Vec<String> = type_args
                        .iter()
                        .map(|t| self.mangle_type_name(*t))
                        .collect();
                    let mangled = format!("{}<{}>", name, type_arg_names.join(","));
                    (mangled, Some(name.clone()), Some(type_args.clone()))
                }
                ResolvedType::BuiltinArray(elem) => {
                    let elem_name = self.mangle_type_name(elem);
                    let mangled = format!("Array<{}>", elem_name);
                    (mangled, Some("Array".to_string()), Some(vec![elem]))
                }
                _ => (self.mangle_type_name(base_type_id), None, None),
            };

        // Build mangled method name:
        // - Trait method: StructName^TraitName::method_name
        // - Inherent method: StructName::method_name
        let mangled_method_name = match &trait_name {
            Some(trait_n) => format!(
                "{}^{}::{}",
                receiver_struct_name, trait_n, method_call.method
            ),
            None => format!("{}::{}", receiver_struct_name, method_call.method),
        };

        // Build monomorph_info for method calls on generic types
        let monomorph_info =
            if let (Some(base), Some(type_args)) = (base_struct_name, receiver_type_args) {
                let generic_name = format!("{}::{}", base, method_call.method);
                Some(MonomorphInfo {
                    generic_name,
                    type_args,
                })
            } else {
                None
            };

        TirExpr::new(
            TirExprKind::MethodCall {
                receiver: Box::new(receiver),
                func: FunctionRef::External {
                    module_path: self.current_module_path.clone(),
                    name: mangled_method_name,
                    monomorph_info,
                },
                type_args,
                args,
            },
            return_type,
            method_call.span,
        )
    }

    /// Resolve a static method call: `Array::<i32>::with_capacity(100)` or `Point::origin()`
    fn resolve_static_method_call(
        &mut self,
        static_call: &ast::StaticMethodCallExpr,
        ctx: &mut FunctionContext,
    ) -> TirExpr {
        // Resolve arguments
        let args: Vec<TirExpr> = static_call
            .args
            .iter()
            .map(|a| self.resolve_expr(a, ctx))
            .collect();

        // Resolve the target type to get struct name, module path, and type args (for generics)
        let target_type_id = self.resolve_type(&static_call.target_type);

        // Special handling for Option::Some and Option::None
        if let ResolvedType::Option(inner_type) =
            self.type_table.borrow().get(target_type_id).clone()
        {
            match static_call.method.as_str() {
                "Some" => {
                    // Option::Some(value) - wrap in OptionSome
                    if args.len() != 1 {
                        self.errors.push(TypeError::ArgumentCountMismatch {
                            expected: 1,
                            found: args.len(),
                            span: static_call.span,
                        });
                        return TirExpr::new(TirExprKind::Unit, TypeTable::ERROR, static_call.span);
                    }
                    let value = args.into_iter().next().unwrap();
                    // Return type is Option<T> where T is the inner type
                    return TirExpr::new(
                        TirExprKind::OptionSome {
                            value: Box::new(value),
                        },
                        target_type_id,
                        static_call.span,
                    );
                }
                "None" => {
                    // Option::None - return null with Option<T> type
                    if !args.is_empty() {
                        self.errors.push(TypeError::ArgumentCountMismatch {
                            expected: 0,
                            found: args.len(),
                            span: static_call.span,
                        });
                        return TirExpr::new(TirExprKind::Unit, TypeTable::ERROR, static_call.span);
                    }
                    // Inner type comes from the Option type annotation
                    let _ = inner_type; // Used to verify type is known
                    return TirExpr::new(TirExprKind::Null, target_type_id, static_call.span);
                }
                _ => {
                    // Other Option methods are not yet supported
                }
            }
        }

        // Handle custom variant construction: Shape::Circle(5.0) or MyVariant::Unit
        if let ResolvedType::Variant {
            name,
            module_path: _,
        } = self.type_table.borrow().get(target_type_id).clone()
        {
            // Look up the variant case info
            if let Some((_, _type_params, cases)) = self.variant_cases.get(&name) {
                // Find the case by name
                if let Some((case_index, (case_name, field_types))) = cases
                    .iter()
                    .enumerate()
                    .find(|(_, (n, _))| n == &static_call.method)
                {
                    // Validate argument count
                    if args.len() != field_types.len() {
                        self.errors.push(TypeError::ArgumentCountMismatch {
                            expected: field_types.len(),
                            found: args.len(),
                            span: static_call.span,
                        });
                        return TirExpr::new(TirExprKind::Unit, TypeTable::ERROR, static_call.span);
                    }

                    // Create VariantConstruct expression
                    return TirExpr::new(
                        TirExprKind::VariantConstruct {
                            variant_type: target_type_id,
                            case_index: case_index as u32,
                            case_name: case_name.clone(),
                            fields: args,
                        },
                        target_type_id,
                        static_call.span,
                    );
                } else {
                    // Unknown case name
                    self.errors.push(TypeError::UnknownFunction {
                        name: format!("{}::{}", name, static_call.method),
                        span: static_call.span,
                    });
                    return TirExpr::new(TirExprKind::Unit, TypeTable::ERROR, static_call.span);
                }
            } else {
                // Variant not found in variant_cases (shouldn't happen)
                self.errors.push(TypeError::UnknownType {
                    name: name.clone(),
                    span: static_call.span,
                });
                return TirExpr::new(TirExprKind::Unit, TypeTable::ERROR, static_call.span);
            }
        }

        let (struct_name, module_path, mangled_struct_name, struct_type_args) =
            match self.type_table.borrow().get(target_type_id) {
                ResolvedType::Struct { name, module_path } => {
                    (name.clone(), module_path.clone(), name.clone(), vec![])
                }
                ResolvedType::GenericInstance {
                    name,
                    module_path,
                    type_args,
                } => {
                    // Build mangled name for generic type: Array<i32>
                    let type_arg_names: Vec<String> = type_args
                        .iter()
                        .map(|t| self.mangle_type_name(*t))
                        .collect();
                    let mangled = format!("{}<{}>", name, type_arg_names.join(","));
                    (
                        name.clone(),
                        module_path.clone(),
                        mangled,
                        type_args.clone(),
                    )
                }
                _ => {
                    // Unknown type - return error expression
                    return TirExpr::new(TirExprKind::Unit, TypeTable::ERROR, static_call.span);
                }
            };

        // Build the mangled function name
        let mangled_func_name = format!("{}::{}", mangled_struct_name, static_call.method);

        // Look up return type
        let mut return_type = self.lookup_static_method_return_type(
            &struct_name,
            &module_path,
            &static_call.method,
            &mangled_func_name,
        );

        // If we have type arguments from a generic type, substitute type parameters in the return type
        if !struct_type_args.is_empty() {
            return_type = self.substitute_type_params(return_type, &struct_type_args);
        }

        // Build monomorph_info for generic instantiations
        let monomorph_info = if !struct_type_args.is_empty() {
            // Generic static method: track the original generic name
            let generic_name = format!("{}::{}", struct_name, static_call.method);
            Some(MonomorphInfo {
                generic_name,
                type_args: struct_type_args,
            })
        } else {
            None
        };

        TirExpr::new(
            TirExprKind::StaticCall {
                func: FunctionRef::External {
                    module_path,
                    name: mangled_func_name,
                    monomorph_info,
                },
                args,
            },
            return_type,
            static_call.span,
        )
    }

    /// Look up static method return type based on struct name and method name
    fn lookup_static_method_return_type(
        &mut self,
        struct_name: &str,
        module_path: &[String],
        method_name: &str,
        mangled_func_name: &str,
    ) -> TypeId {
        // First check locally registered function_return_types
        if let Some(&return_type) = self.function_return_types.get(mangled_func_name) {
            return return_type;
        }

        // Also try with just StructName::method (for non-generic types)
        let simple_name = format!("{}::{}", struct_name, method_name);
        if let Some(&return_type) = self.function_return_types.get(&simple_name) {
            return return_type;
        }

        // Try looking up in loaded modules
        if !module_path.is_empty()
            && let Some(module) = self.loaded_modules.get(module_path)
        {
            for item in &module.items {
                if let Item::Impl(impl_block) = item {
                    let impl_struct_name = self.get_type_name(&impl_block.ty);
                    if impl_struct_name == struct_name {
                        for method in &impl_block.methods {
                            // Static methods have no self parameter
                            let has_self = method
                                .params
                                .iter()
                                .any(|p| p.self_kind != ast::SelfKind::None);
                            if method.name == method_name && !has_self {
                                // Set up type parameters from impl block before resolving
                                let old_type_params = std::mem::take(&mut self.current_type_params);

                                // Extract type params from impl block type (e.g., impl Array<T>)
                                if let ast::Type::Generic(generic) = &impl_block.ty {
                                    for (i, arg) in generic.args.iter().enumerate() {
                                        if let ast::Type::Named(named) = arg {
                                            let name = &named.name;
                                            if !self.current_type_params.contains_key(name) {
                                                let type_id = self
                                                    .type_table
                                                    .borrow_mut()
                                                    .make_type_param(name.clone(), i as u32);
                                                self.current_type_params
                                                    .insert(name.clone(), (i as u32, type_id));
                                            }
                                        }
                                    }
                                }

                                let result = method
                                    .return_type
                                    .as_ref()
                                    .map(|t| self.resolve_type(t))
                                    .unwrap_or(TypeTable::UNIT);

                                // Restore type parameters
                                self.current_type_params = old_type_params;

                                return result;
                            }
                        }
                    }
                }
            }
        }

        // Search all loaded modules if module_path is empty
        if module_path.is_empty() {
            for (_, module) in self.loaded_modules.iter() {
                for item in &module.items {
                    if let Item::Impl(impl_block) = item {
                        let impl_struct_name = self.get_type_name(&impl_block.ty);
                        if impl_struct_name == struct_name {
                            for method in &impl_block.methods {
                                let has_self = method
                                    .params
                                    .iter()
                                    .any(|p| p.self_kind != ast::SelfKind::None);
                                if method.name == method_name && !has_self {
                                    // Set up type parameters from impl block before resolving
                                    let old_type_params =
                                        std::mem::take(&mut self.current_type_params);

                                    // Extract type params from impl block type (e.g., impl Array<T>)
                                    if let ast::Type::Generic(generic) = &impl_block.ty {
                                        for (i, arg) in generic.args.iter().enumerate() {
                                            if let ast::Type::Named(named) = arg {
                                                let name = &named.name;
                                                if !self.current_type_params.contains_key(name) {
                                                    let type_id = self
                                                        .type_table
                                                        .borrow_mut()
                                                        .make_type_param(name.clone(), i as u32);
                                                    self.current_type_params
                                                        .insert(name.clone(), (i as u32, type_id));
                                                }
                                            }
                                        }
                                    }

                                    let result = method
                                        .return_type
                                        .as_ref()
                                        .map(|t| self.resolve_type(t))
                                        .unwrap_or(TypeTable::UNIT);

                                    // Restore type parameters
                                    self.current_type_params = old_type_params;

                                    return result;
                                }
                            }
                        }
                    }
                }
            }
        }

        TypeTable::UNKNOWN
    }

    /// Check if a qualified name `struct_name::method_name` is a static method
    fn is_static_method(&self, struct_name: &str, method_name: &str) -> bool {
        // Build the mangled function name
        let mangled_name = format!("{}::{}", struct_name, method_name);

        // Check if it's registered in function_return_types (static methods are registered there)
        if self.function_return_types.contains_key(&mangled_name) {
            return true;
        }

        // Also check in loaded modules' impl blocks
        for (_, module) in self.loaded_modules.iter() {
            for item in &module.items {
                if let Item::Impl(impl_block) = item {
                    let impl_struct_name = self.get_type_name(&impl_block.ty);
                    if impl_struct_name == struct_name {
                        for method in &impl_block.methods {
                            // Static methods have no self parameter
                            let has_self = method
                                .params
                                .iter()
                                .any(|p| p.self_kind != ast::SelfKind::None);
                            if method.name == method_name && !has_self {
                                return true;
                            }
                        }
                    }
                }
            }
        }

        // Check current module's impl blocks
        for item in &self.current_module_items {
            if let Item::Impl(impl_block) = item {
                let impl_struct_name = self.get_type_name(&impl_block.ty);
                if impl_struct_name == struct_name {
                    for method in &impl_block.methods {
                        let has_self = method
                            .params
                            .iter()
                            .any(|p| p.self_kind != ast::SelfKind::None);
                        if method.name == method_name && !has_self {
                            return true;
                        }
                    }
                }
            }
        }

        false
    }

    /// Resolve a static method call from a qualified name like `Point::origin()`
    fn resolve_static_method_call_from_qualified(
        &mut self,
        struct_name: &str,
        method_name: &str,
        mangled_func_name: &str,
        args: &[TirExpr],
        span: Span,
        _ctx: &mut FunctionContext,
    ) -> TirExpr {
        // Look up return type
        let return_type = self.lookup_static_method_return_type(
            struct_name,
            &[], // Module path will be looked up during lookup
            method_name,
            mangled_func_name,
        );

        // Determine module path for the struct
        let module_path = self.find_struct_module_path(struct_name);

        TirExpr::new(
            TirExprKind::StaticCall {
                func: FunctionRef::External {
                    module_path,
                    name: mangled_func_name.to_string(),
                    monomorph_info: None,
                },
                args: args.to_vec(),
            },
            return_type,
            span,
        )
    }

    /// Find the module path for a struct by name
    fn find_struct_module_path(&self, struct_name: &str) -> Vec<String> {
        // Check current module
        for item in &self.current_module_items {
            if let Item::Struct(s) = item
                && s.name == struct_name
            {
                return self.current_module_path.clone();
            }
        }

        // Check loaded modules
        for (path, module) in self.loaded_modules.iter() {
            for item in &module.items {
                if let Item::Struct(s) = item
                    && s.name == struct_name
                {
                    return path.clone();
                }
            }
        }

        // Default to current module path
        self.current_module_path.clone()
    }

    /// Mangle a type name for use in function names
    fn mangle_type_name(&self, type_id: TypeId) -> String {
        match self.type_table.borrow().get(type_id) {
            ResolvedType::Primitive(prim) => match prim {
                crate::tir::PrimitiveType::I8 => "i8".to_string(),
                crate::tir::PrimitiveType::I16 => "i16".to_string(),
                crate::tir::PrimitiveType::I32 => "i32".to_string(),
                crate::tir::PrimitiveType::I64 => "i64".to_string(),
                crate::tir::PrimitiveType::I128 => "i128".to_string(),
                crate::tir::PrimitiveType::U8 => "u8".to_string(),
                crate::tir::PrimitiveType::U16 => "u16".to_string(),
                crate::tir::PrimitiveType::U32 => "u32".to_string(),
                crate::tir::PrimitiveType::U64 => "u64".to_string(),
                crate::tir::PrimitiveType::U128 => "u128".to_string(),
                crate::tir::PrimitiveType::F32 => "f32".to_string(),
                crate::tir::PrimitiveType::F64 => "f64".to_string(),
                crate::tir::PrimitiveType::Bool => "bool".to_string(),
                crate::tir::PrimitiveType::Char => "char".to_string(),
            },
            ResolvedType::Unit => "unit".to_string(),
            ResolvedType::String => "String".to_string(),
            ResolvedType::Struct { name, .. } => name.clone(),
            ResolvedType::GenericInstance {
                name, type_args, ..
            } => {
                let args: Vec<String> = type_args
                    .iter()
                    .map(|t| self.mangle_type_name(*t))
                    .collect();
                format!("{}<{}>", name, args.join(","))
            }
            ResolvedType::Option(inner) => format!("Option<{}>", self.mangle_type_name(*inner)),
            ResolvedType::Ref(inner) => format!("ref<{}>", self.mangle_type_name(*inner)),
            ResolvedType::MutRef(inner) => format!("mutref<{}>", self.mangle_type_name(*inner)),
            ResolvedType::TypeParam { name, .. } => name.clone(),
            ResolvedType::Tuple(elems) => {
                let parts: Vec<String> = elems.iter().map(|e| self.mangle_type_name(*e)).collect();
                format!("Tuple<{}>", parts.join(","))
            }
            ResolvedType::Function {
                params,
                return_type,
                ..
            } => {
                let ret_name = self.mangle_type_name(*return_type);
                format!("Fn<{},{}>", params.len(), ret_name)
            }
            ResolvedType::BuiltinArray(elem) => {
                format!("Array<{}>", self.mangle_type_name(*elem))
            }
            _ => "unknown".to_string(),
        }
    }

    /// Look up method info based on receiver type and method name.
    /// Returns MethodInfo including return type and self_kind, or None if not found.
    fn lookup_method_info(&self, receiver_type: TypeId, method_name: &str) -> Option<MethodInfo> {
        // First, get the base (non-reference) type for method lookup
        let base_type_id = self.get_base_type(receiver_type);
        let base_type = self.type_table.borrow().get(base_type_id).clone();

        // Get the struct name and module path from the base type
        let (struct_name, module_path) = match &base_type {
            ResolvedType::Struct { name, module_path } => (name.clone(), module_path.clone()),
            // Generic instances like Box<i32> use the base name "Box" for method lookup
            ResolvedType::GenericInstance {
                name, module_path, ..
            } => (name.clone(), module_path.clone()),
            // Primitive types have built-in methods like to_string()
            ResolvedType::Primitive(_) => {
                if method_name == "to_string" {
                    // Return String struct type - primitives use value receiver
                    let return_type = self
                        .type_table
                        .borrow()
                        .find_struct_type("String", &string_module_path())
                        .unwrap_or(TypeTable::UNKNOWN);
                    return Some(MethodInfo {
                        return_type,
                        self_kind: ast::SelfKind::None,
                    });
                }
                return None;
            }
            // String type (legacy - String is now a struct, but handle for backwards compat)
            ResolvedType::String => match method_name {
                "len" => {
                    return Some(MethodInfo {
                        return_type: TypeTable::I32,
                        self_kind: ast::SelfKind::Ref,
                    });
                }
                "get" => {
                    return Some(MethodInfo {
                        return_type: TypeTable::I32,
                        self_kind: ast::SelfKind::Ref,
                    });
                }
                "set" => {
                    return Some(MethodInfo {
                        return_type: TypeTable::UNIT,
                        self_kind: ast::SelfKind::MutRef,
                    });
                }
                _ => return None,
            },
            _ => return None,
        };

        // Build the mangled method name and look it up locally first
        let mangled_name = format!("{}::{}", struct_name, method_name);
        if let Some(&return_type) = self.function_return_types.get(&mangled_name) {
            // For locally registered methods, we need to find the self_kind from the AST
            // Check current module items for the method definition
            if let Some(self_kind) = self.find_method_self_kind(&struct_name, method_name) {
                return Some(MethodInfo {
                    return_type,
                    self_kind,
                });
            }
            // Fallback: assume &self for methods (most common case)
            return Some(MethodInfo {
                return_type,
                self_kind: ast::SelfKind::Ref,
            });
        }

        // Try looking up in loaded modules (for imported structs)
        // Only check inherent impls (not trait impls) - trait impls are handled separately
        if !module_path.is_empty()
            && let Some(module) = self.loaded_modules.get(&module_path)
        {
            for item in &module.items {
                if let Item::Impl(impl_block) = item {
                    // Skip trait impls - only look at inherent impls
                    if impl_block.trait_type.is_some() {
                        continue;
                    }
                    let impl_struct_name = self.get_type_name(&impl_block.ty);
                    if impl_struct_name == struct_name {
                        for method in &impl_block.methods {
                            if method.name == method_name {
                                let return_type = method
                                    .return_type
                                    .as_ref()
                                    .map(|t| self.resolve_type_no_register(t))
                                    .unwrap_or(TypeTable::UNIT);
                                let self_kind = method
                                    .params
                                    .first()
                                    .map(|p| p.self_kind)
                                    .unwrap_or(ast::SelfKind::None);
                                return Some(MethodInfo {
                                    return_type,
                                    self_kind,
                                });
                            }
                        }
                    }
                }
            }
        }

        // Search all loaded modules if module_path is empty (for prelude types)
        // Only check inherent impls (not trait impls) - trait impls are handled separately
        if module_path.is_empty() {
            for (_, module) in self.loaded_modules.iter() {
                for item in &module.items {
                    if let Item::Impl(impl_block) = item {
                        // Skip trait impls - only look at inherent impls
                        if impl_block.trait_type.is_some() {
                            continue;
                        }
                        let impl_struct_name = self.get_type_name(&impl_block.ty);
                        if impl_struct_name == struct_name {
                            for method in &impl_block.methods {
                                if method.name == method_name {
                                    let return_type = method
                                        .return_type
                                        .as_ref()
                                        .map(|t| self.resolve_type_no_register(t))
                                        .unwrap_or(TypeTable::UNIT);
                                    let self_kind = method
                                        .params
                                        .first()
                                        .map(|p| p.self_kind)
                                        .unwrap_or(ast::SelfKind::None);
                                    return Some(MethodInfo {
                                        return_type,
                                        self_kind,
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }

        None
    }

    /// Find the self_kind for a method in current module items
    fn find_method_self_kind(&self, struct_name: &str, method_name: &str) -> Option<ast::SelfKind> {
        for item in &self.current_module_items {
            if let Item::Impl(impl_block) = item {
                // Skip trait impls
                if impl_block.trait_type.is_some() {
                    continue;
                }
                let impl_struct_name = self.get_type_name(&impl_block.ty);
                if impl_struct_name == struct_name {
                    for method in &impl_block.methods {
                        if method.name == method_name {
                            return method.params.first().map(|p| p.self_kind);
                        }
                    }
                }
            }
        }
        None
    }

    /// Get the base (non-reference) type by stripping all Ref/MutRef wrappers
    fn get_base_type(&self, type_id: TypeId) -> TypeId {
        let mut current = type_id;
        loop {
            match self.type_table.borrow().get(current).clone() {
                ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
                    current = inner;
                }
                _ => return current,
            }
        }
    }

    /// Adjust the receiver expression to match what the method's self parameter expects
    fn adjust_receiver_for_self_kind(
        &mut self,
        receiver: TirExpr,
        self_kind: ast::SelfKind,
        span: Span,
    ) -> TirExpr {
        let receiver_type = self.type_table.borrow().get(receiver.type_id).clone();

        match self_kind {
            ast::SelfKind::None => {
                // Method expects value (self), so deref all refs
                self.deref_to_value(receiver, span)
            }
            ast::SelfKind::Ref => {
                // Method expects &self
                match &receiver_type {
                    ResolvedType::Ref(_) => {
                        // Already &T, use as-is
                        receiver
                    }
                    ResolvedType::MutRef(_) => {
                        // &mut T can be coerced to &T, use as-is
                        receiver
                    }
                    _ => {
                        // Value T, need to add &
                        let ref_type = self.type_table.borrow_mut().make_ref(receiver.type_id);
                        TirExpr::new(
                            TirExprKind::Unary {
                                op: TirUnaryOp::Ref,
                                expr: Box::new(receiver),
                            },
                            ref_type,
                            span,
                        )
                    }
                }
            }
            ast::SelfKind::MutRef => {
                // Method expects &mut self
                match &receiver_type {
                    ResolvedType::MutRef(_) => {
                        // Already &mut T, use as-is
                        receiver
                    }
                    _ => {
                        // Value T, need to add &mut
                        let mut_ref_type =
                            self.type_table.borrow_mut().make_mut_ref(receiver.type_id);
                        TirExpr::new(
                            TirExprKind::Unary {
                                op: TirUnaryOp::MutRef,
                                expr: Box::new(receiver),
                            },
                            mut_ref_type,
                            span,
                        )
                    }
                }
            }
        }
    }

    /// Dereference a receiver until it's a value (non-reference) type
    fn deref_to_value(&self, mut receiver: TirExpr, span: Span) -> TirExpr {
        loop {
            match self.type_table.borrow().get(receiver.type_id).clone() {
                ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
                    receiver = TirExpr::new(
                        TirExprKind::Unary {
                            op: TirUnaryOp::Deref,
                            expr: Box::new(receiver),
                        },
                        inner,
                        span,
                    );
                }
                _ => return receiver,
            }
        }
    }

    /// Find a trait method for a given type and method name.
    /// Returns (trait_name, MethodInfo) if found, None otherwise.
    /// This is used when an inherent method is not found.
    fn find_trait_method_for_type(
        &mut self,
        struct_name: &str,
        method_name: &str,
        module_path: &[String],
    ) -> Option<(String, MethodInfo)> {
        let mut found_traits: Vec<(String, MethodInfo)> = Vec::new();

        // Collect impl blocks to check (avoiding borrow issues)
        let mut impl_blocks_to_check: Vec<(Type, Type, Vec<Function>)> = Vec::new();

        // Check specific module if provided
        if !module_path.is_empty()
            && let Some(module) = self.loaded_modules.get(module_path)
        {
            for item in &module.items {
                if let Item::Impl(impl_block) = item
                    && let Some(trait_type) = &impl_block.trait_type
                {
                    impl_blocks_to_check.push((
                        impl_block.ty.clone(),
                        trait_type.clone(),
                        impl_block.methods.clone(),
                    ));
                }
            }
        }

        // Also check all loaded modules
        for (_, module) in self.loaded_modules.iter() {
            for item in &module.items {
                if let Item::Impl(impl_block) = item
                    && let Some(trait_type) = &impl_block.trait_type
                {
                    impl_blocks_to_check.push((
                        impl_block.ty.clone(),
                        trait_type.clone(),
                        impl_block.methods.clone(),
                    ));
                }
            }
        }

        // Check current module items
        for item in &self.current_module_items {
            if let Item::Impl(impl_block) = item
                && let Some(trait_type) = &impl_block.trait_type
            {
                impl_blocks_to_check.push((
                    impl_block.ty.clone(),
                    trait_type.clone(),
                    impl_block.methods.clone(),
                ));
            }
        }

        // Now process the collected impl blocks with mutable access
        for (impl_ty, trait_type, methods) in impl_blocks_to_check {
            let impl_struct_name = self.get_type_name(&impl_ty);
            if impl_struct_name == struct_name {
                for method in &methods {
                    if method.name == method_name {
                        let trait_name = self.get_type_name(&trait_type);
                        let return_type = method
                            .return_type
                            .as_ref()
                            .map(|t| self.resolve_type(t))
                            .unwrap_or(TypeTable::UNIT);
                        let self_kind = method
                            .params
                            .first()
                            .map(|p| p.self_kind)
                            .unwrap_or(ast::SelfKind::None);
                        found_traits.push((
                            trait_name,
                            MethodInfo {
                                return_type,
                                self_kind,
                            },
                        ));
                    }
                }
            }
        }

        // Remove duplicates
        found_traits.dedup_by(|a, b| a.0 == b.0);

        // Return the first one found (if there are multiple, it would be ambiguous,
        // but we'll handle that later with explicit disambiguation syntax)
        found_traits.into_iter().next()
    }

    /// Resolve a field access
    fn resolve_field_access(
        &mut self,
        field_access: &ast::FieldAccessExpr,
        ctx: &mut FunctionContext,
    ) -> TirExpr {
        let expr = self.resolve_expr(&field_access.expr, ctx);

        // Look up field type from struct type
        let (field_index, field_type) =
            self.lookup_field_type(expr.type_id, &field_access.field, field_access.span);

        TirExpr::new(
            TirExprKind::FieldAccess {
                expr: Box::new(expr),
                field_index,
                field_name: field_access.field.clone(),
            },
            field_type,
            field_access.span,
        )
    }

    /// Look up field type from a struct or tuple type
    fn lookup_field_type(
        &mut self,
        struct_type: TypeId,
        field_name: &str,
        _span: Span,
    ) -> (u32, TypeId) {
        // Clone the type to avoid borrow issues
        let resolved = self.type_table.borrow().get(struct_type).clone();
        match resolved {
            // Struct field access
            ResolvedType::Struct { name, .. } => {
                if let Some((_, fields)) = self.struct_fields.get(&name) {
                    for (index, (fname, ftype)) in fields.iter().enumerate() {
                        if fname == field_name {
                            return (index as u32, *ftype);
                        }
                    }
                }
            }
            // Tuple field access (numeric field names: 0, 1, 2, ...)
            ResolvedType::Tuple(elements) => {
                if let Ok(index) = field_name.parse::<usize>()
                    && index < elements.len()
                {
                    return (index as u32, elements[index]);
                }
            }
            // Reference types - look through to inner type
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
                return self.lookup_field_type(inner, field_name, _span);
            }
            // Generic instance - look up field from generic struct definition
            // and substitute type parameters with concrete type args
            ResolvedType::GenericInstance {
                name, type_args, ..
            } => {
                // Clone fields to avoid borrow issues
                let fields_clone = self.struct_fields.get(&name).cloned();
                if let Some((_, fields)) = fields_clone {
                    for (index, (fname, ftype)) in fields.iter().enumerate() {
                        if fname == field_name {
                            // Substitute type parameters with concrete types
                            let concrete_type = self.substitute_type_params(*ftype, &type_args);
                            return (index as u32, concrete_type);
                        }
                    }
                }
            }
            _ => {}
        }
        (0, TypeTable::UNKNOWN)
    }

    /// Substitute type parameters in a type with concrete type arguments
    fn substitute_type_params(&mut self, type_id: TypeId, type_args: &[TypeId]) -> TypeId {
        let resolved_type = self.type_table.borrow().get(type_id).clone();
        match resolved_type {
            ResolvedType::TypeParam { index, .. } => {
                // Direct substitution: T -> type_args[index]
                type_args.get(index as usize).copied().unwrap_or(type_id)
            }
            ResolvedType::BuiltinArray(elem) => {
                let new_elem = self.substitute_type_params(elem, type_args);
                self.type_table
                    .borrow_mut()
                    .intern(ResolvedType::BuiltinArray(new_elem))
            }
            ResolvedType::Option(inner) => {
                let new_inner = self.substitute_type_params(inner, type_args);
                self.type_table.borrow_mut().make_option(new_inner)
            }
            ResolvedType::Ref(inner) => {
                let new_inner = self.substitute_type_params(inner, type_args);
                self.type_table.borrow_mut().make_ref(new_inner)
            }
            ResolvedType::MutRef(inner) => {
                let new_inner = self.substitute_type_params(inner, type_args);
                self.type_table.borrow_mut().make_mut_ref(new_inner)
            }
            ResolvedType::Tuple(elems) => {
                let new_elems: Vec<TypeId> = elems
                    .iter()
                    .map(|&e| self.substitute_type_params(e, type_args))
                    .collect();
                self.type_table.borrow_mut().make_tuple(new_elems)
            }
            ResolvedType::GenericInstance {
                name,
                module_path,
                type_args: inner_args,
            } => {
                // Recursively substitute in nested generic instances
                let new_args: Vec<TypeId> = inner_args
                    .iter()
                    .map(|&arg| self.substitute_type_params(arg, type_args))
                    .collect();
                self.type_table
                    .borrow_mut()
                    .make_generic_instance(name, module_path, new_args)
            }
            // Other types don't contain type parameters
            _ => type_id,
        }
    }

    /// Resolve an index expression
    fn resolve_index(&mut self, index: &ast::IndexExpr, ctx: &mut FunctionContext) -> TirExpr {
        let expr = self.resolve_expr(&index.expr, ctx);

        // Get base type (unwrap reference if needed)
        let base_type_id = match self.type_table.borrow().get(expr.type_id) {
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => *inner,
            _ => expr.type_id,
        };
        let base_type = self.type_table.borrow().get(base_type_id).clone();

        // Handle tuple indexing: t[0] is equivalent to t.0
        if let ResolvedType::Tuple(elements) = base_type {
            let elements = elements.clone();
            // Tuple indexing requires a constant integer index
            if let ast::Expr::Literal(ast::LiteralExpr {
                value: ast::Literal::Int(int_lit),
                ..
            }) = &index.index
                && let Ok(idx) = int_lit.repr.parse::<usize>()
            {
                if idx < elements.len() {
                    let field_type = elements[idx];
                    return TirExpr::new(
                        TirExprKind::FieldAccess {
                            expr: Box::new(expr),
                            field_index: idx as u32,
                            field_name: idx.to_string(),
                        },
                        field_type,
                        index.span,
                    );
                } else {
                    self.errors.push(TypeError::InvalidLiteral {
                        message: format!(
                            "tuple index {} out of bounds, tuple has {} elements",
                            idx,
                            elements.len()
                        ),
                        span: index.span,
                    });
                    // Return a placeholder expression with unknown type
                    return TirExpr::new(TirExprKind::Unit, TypeTable::UNKNOWN, index.span);
                }
            }
            // Non-constant index on tuple
            self.errors.push(TypeError::InvalidLiteral {
                message: "tuple index must be a constant integer".to_string(),
                span: index.span,
            });
            return TirExpr::new(TirExprKind::Unit, TypeTable::UNKNOWN, index.span);
        }

        // Array indexing
        let element_type = self
            .type_table
            .borrow()
            .as_array(expr.type_id)
            .unwrap_or(TypeTable::UNKNOWN);
        let index_expr = self.resolve_expr(&index.index, ctx);

        TirExpr::new(
            TirExprKind::Index {
                expr: Box::new(expr),
                index: Box::new(index_expr),
            },
            element_type,
            index.span,
        )
    }

    /// Resolve an if expression
    fn resolve_if_expr(&mut self, if_expr: &IfExpr, ctx: &mut FunctionContext) -> TirExpr {
        // Handle optional init binding (scoped to this if expression)
        if if_expr.init.is_some() {
            ctx.enter_scope();
        }

        // Note: init binding for if expressions would need special handling
        // in codegen since expressions can't contain statements. For now,
        // we just resolve it for scoping purposes but codegen doesn't support it.
        if let Some(init) = &if_expr.init {
            // The let binding is resolved for name resolution but not emitted as TIR
            // This is a limitation for now - full support would require block expressions
            let _init_stmt = self.resolve_let(init, ctx);
        }

        // Resolve the condition
        let condition = match &if_expr.condition {
            ast::IfCondition::Expr(expr) => self.resolve_expr(expr, ctx),
            ast::IfCondition::Pattern { span, .. } => {
                self.errors.push(TypeError::NotYetImplemented {
                    feature: "pattern matching in if expressions (use if statement instead)"
                        .to_string(),
                    span: *span,
                });
                TirExpr::new(TirExprKind::BoolLiteral(true), TypeTable::BOOL, *span)
            }
        };

        let then_block = self.resolve_block(&if_expr.then_block, ctx);
        let else_block = if_expr
            .else_block
            .as_ref()
            .map(|b| self.resolve_block(b, ctx));

        // If expression type is the type of the branches
        let type_id = then_block
            .stmts
            .last()
            .and_then(|s| match &s.kind {
                TirStmtKind::Expr(e) => Some(e.type_id),
                _ => None,
            })
            .unwrap_or(TypeTable::UNIT);

        let result = TirExpr::new(
            TirExprKind::If {
                condition: Box::new(condition),
                then_branch: then_block,
                else_branch: else_block,
            },
            type_id,
            if_expr.span,
        );

        if if_expr.init.is_some() {
            ctx.exit_scope();
        }

        result
    }

    /// Resolve a match expression
    fn resolve_match_expr(
        &mut self,
        match_expr: &ast::MatchExpr,
        ctx: &mut FunctionContext,
    ) -> TirExpr {
        let expr = self.resolve_expr(&match_expr.expr, ctx);
        let arms: Vec<TirMatchArm> = match_expr
            .arms
            .iter()
            .map(|arm| self.resolve_match_arm(arm, ctx))
            .collect();

        // Match expression type is the type of the first arm body
        let type_id = arms
            .first()
            .map(|a| a.body.type_id)
            .unwrap_or(TypeTable::UNIT);

        TirExpr::new(
            TirExprKind::Match {
                expr: Box::new(expr),
                arms,
            },
            type_id,
            match_expr.span,
        )
    }

    /// Resolve a match arm
    fn resolve_match_arm(&mut self, arm: &MatchArm, ctx: &mut FunctionContext) -> TirMatchArm {
        let pattern = self.resolve_pattern(&arm.pattern, ctx);
        let body = self.resolve_expr(&arm.body, ctx);

        TirMatchArm {
            pattern,
            body,
            span: arm.span,
        }
    }

    /// Resolve a pattern
    fn resolve_pattern(&mut self, pattern: &Pattern, ctx: &mut FunctionContext) -> TirPattern {
        match pattern {
            Pattern::Wildcard => TirPattern::Wildcard,
            Pattern::Ident(name) => {
                // Create a local for the binding
                let index = ctx.add_local(name.clone(), TypeTable::UNKNOWN, false);
                TirPattern::Binding {
                    name: name.clone(),
                    local_index: index,
                }
            }
            Pattern::Literal(lit) => {
                let tir_lit = match lit {
                    Literal::Int(i) => {
                        // Parse the integer literal
                        match Self::parse_int_literal(&i.repr) {
                            Ok(value) => TirLiteralPattern::Int(value),
                            Err(_) => {
                                // Error was already reported during literal resolution
                                TirLiteralPattern::Int(0)
                            }
                        }
                    }
                    Literal::Bool(b) => TirLiteralPattern::Bool(*b),
                    Literal::Char(c) => TirLiteralPattern::Char(*c),
                    Literal::String(s) => TirLiteralPattern::String(s.clone()),
                    Literal::Null => TirLiteralPattern::Null,
                    _ => TirLiteralPattern::Null,
                };
                TirPattern::Literal(tir_lit)
            }
            Pattern::Tuple(patterns) => {
                let resolved: Vec<TirPattern> = patterns
                    .iter()
                    .map(|p| self.resolve_pattern(p, ctx))
                    .collect();
                TirPattern::Tuple(resolved)
            }
            Pattern::Variant {
                variant_name,
                bindings,
                ..
            } => {
                let resolved_bindings: Vec<TirPattern> = bindings
                    .iter()
                    .map(|p| self.resolve_pattern(p, ctx))
                    .collect();
                TirPattern::Variant {
                    enum_type: TypeTable::UNKNOWN, // Will be inferred during type checking
                    variant_name: variant_name.clone(),
                    bindings: resolved_bindings,
                }
            }
        }
    }

    /// Resolve a closure
    fn resolve_closure(
        &mut self,
        closure: &ast::ClosureExpr,
        ctx: &mut FunctionContext,
    ) -> TirExpr {
        // Create a closure context with access to outer scope for capture detection
        let mut closure_ctx = FunctionContext::new_closure(TypeTable::UNKNOWN, ctx);

        // Add closure parameters
        let params: Vec<(String, TypeId)> = closure
            .params
            .iter()
            .map(|p| {
                let type_id =
                    p.ty.as_ref()
                        .map(|t| self.resolve_type(t))
                        .unwrap_or(TypeTable::UNKNOWN);
                closure_ctx.add_local(p.name.clone(), type_id, false);
                (p.name.clone(), type_id)
            })
            .collect();

        // Resolve body - this will detect captured variables
        let body = self.resolve_expr(&closure.body, &mut closure_ctx);

        // Build capture list from detected captures
        let captures: Vec<TirCapture> = closure_ctx
            .get_captures()
            .into_iter()
            .map(|(name, _index, local)| TirCapture {
                name,
                outer_index: local.index,
                type_id: local.type_id,
                is_mut: local.is_mut,
            })
            .collect();

        // Determine return type:
        // - For block bodies, check for return statements
        // - Fall back to the body expression's type
        let return_type = if let TirExprKind::Block(ref block) = body.kind {
            Self::find_return_type_in_block(block).unwrap_or(body.type_id)
        } else {
            body.type_id
        };

        // Create function type
        let param_types: Vec<TypeId> = params.iter().map(|(_, t)| *t).collect();
        let func_type =
            self.type_table
                .borrow_mut()
                .make_function(param_types, return_type, Vec::new());

        TirExpr::new(
            TirExprKind::Closure {
                params,
                body: Box::new(body),
                captures,
            },
            func_type,
            closure.span,
        )
    }

    /// Resolve a template string
    /// Resolve a template string - desugars to string concatenation
    /// `Hello, {name}!` → string_concat("Hello, ", to_string(name), "!")
    fn resolve_template_string(
        &mut self,
        template: &ast::TemplateStringExpr,
        ctx: &mut FunctionContext,
    ) -> TirExpr {
        // Get the String struct type
        let string_type = self.get_string_struct_type();

        // Collect all parts as expressions
        let mut parts: Vec<TirExpr> = Vec::new();

        for part in &template.parts {
            match part {
                ast::TemplatePart::String(s) => {
                    if !s.is_empty() {
                        parts.push(TirExpr::new(
                            TirExprKind::StringLiteral(s.clone()),
                            string_type,
                            template.span,
                        ));
                    }
                }
                ast::TemplatePart::Interpolation { expr, format: _ } => {
                    // Resolve the expression
                    let resolved = self.resolve_expr(expr, ctx);
                    // TODO: handle format specifiers
                    // For now, wrap in to_string if not already a string
                    let string_expr = if resolved.type_id == string_type {
                        resolved
                    } else {
                        // Call to_string method
                        let receiver_type_name = self.mangle_type_name(resolved.type_id);
                        let mangled_method_name = format!("{}::to_string", receiver_type_name);
                        TirExpr::new(
                            TirExprKind::MethodCall {
                                receiver: Box::new(resolved.clone()),
                                func: FunctionRef::External {
                                    module_path: self.current_module_path.clone(),
                                    name: mangled_method_name,
                                    monomorph_info: None,
                                },
                                type_args: vec![],
                                args: vec![],
                            },
                            string_type,
                            expr.span(),
                        )
                    };
                    parts.push(string_expr);
                }
            }
        }

        // If only one part, return it directly
        if parts.len() == 1 {
            return parts.remove(0);
        }

        // If empty, return empty string
        if parts.is_empty() {
            return TirExpr::new(
                TirExprKind::StringLiteral(String::new()),
                string_type,
                template.span,
            );
        }

        // Build a chain of pairwise string concatenations: concat(concat(a, b), c)
        // string_concat only takes 2 arguments, so we chain them
        let mut result = parts.remove(0);
        for part in parts {
            result = TirExpr::new(
                TirExprKind::Call {
                    func: FunctionRef::External {
                        module_path: vec!["core".to_string(), "internal".to_string()],
                        name: "string_concat".to_string(),
                        monomorph_info: None,
                    },
                    type_args: vec![],
                    args: vec![result, part],
                },
                string_type,
                template.span,
            );
        }
        result
    }

    /// Resolve a cast expression
    fn resolve_cast(&mut self, cast: &ast::CastExpr, ctx: &mut FunctionContext) -> TirExpr {
        let target_type = self.resolve_type(&cast.target_type);

        // Special case: tuple literal cast to Array<T>
        // [1, 2, 3] as Array<i32> should become an ArrayLiteral, not a Cast of TupleLiteral
        let element_type_opt = self.type_table.borrow().as_array(target_type);
        if let ast::Expr::TupleLiteral(tuple_lit) = &cast.expr
            && let Some(element_type) = element_type_opt
        {
            // Resolve each element and check type compatibility
            let elements: Vec<TirExpr> = tuple_lit
                .elements
                .iter()
                .map(|elem| {
                    let resolved = self.resolve_expr(elem, ctx);
                    // Type check: each element must match Array element type
                    if resolved.type_id != element_type && resolved.type_id != TypeTable::UNKNOWN {
                        self.errors.push(TypeError::TypeMismatch {
                            expected: self.type_table.borrow().type_name(element_type),
                            found: self.type_table.borrow().type_name(resolved.type_id),
                            span: elem.span(),
                        });
                    }
                    resolved
                })
                .collect();

            return TirExpr::new(
                TirExprKind::ArrayLiteral { elements },
                target_type,
                cast.span,
            );
        }

        // Normal cast
        let expr = self.resolve_expr(&cast.expr, ctx);

        TirExpr::new(
            TirExprKind::Cast {
                expr: Box::new(expr),
                target_type,
            },
            target_type,
            cast.span,
        )
    }

    /// Find the return type from return statements in a block.
    /// Returns the type of the first return statement found, or None if no returns.
    fn find_return_type_in_block(block: &TirBlock) -> Option<TypeId> {
        for stmt in &block.stmts {
            if let Some(type_id) = Self::find_return_type_in_stmt(stmt) {
                return Some(type_id);
            }
        }
        None
    }

    fn find_return_type_in_stmt(stmt: &TirStmt) -> Option<TypeId> {
        match &stmt.kind {
            TirStmtKind::Return { value: Some(expr) } => Some(expr.type_id),
            TirStmtKind::Return { value: None } => Some(TypeTable::UNIT),
            TirStmtKind::If {
                then_block,
                else_block,
                ..
            } => {
                if let Some(t) = Self::find_return_type_in_block(then_block) {
                    return Some(t);
                }
                if let Some(else_blk) = else_block
                    && let Some(t) = Self::find_return_type_in_block(else_blk)
                {
                    return Some(t);
                }
                None
            }
            TirStmtKind::While { body, .. }
            | TirStmtKind::For { body, .. }
            | TirStmtKind::ForOf { body, .. }
            | TirStmtKind::Loop { body } => Self::find_return_type_in_block(body),
            _ => None,
        }
    }

    /// Resolve a struct literal
    fn resolve_struct_literal(
        &mut self,
        struct_lit: &ast::StructLiteralExpr,
        ctx: &mut FunctionContext,
    ) -> TirExpr {
        // Handle implicit struct literals (name is None)
        let Some(name) = &struct_lit.name else {
            // Implicit struct literal without type context - error
            self.errors.push(TypeError::TypeMismatch {
                expected: "named struct literal (e.g., Point { x: 1, y: 2 })".into(),
                found: "implicit struct literal without type context".into(),
                span: struct_lit.span,
            });
            // Return a dummy expression with unknown type
            return TirExpr::new(
                TirExprKind::IntLiteral {
                    value: 0,
                    repr: "0".into(),
                },
                TypeTable::UNKNOWN,
                struct_lit.span,
            );
        };

        // Look up the struct in the symbol table to resolve imports/aliases
        let (struct_name, module_path) = if let Some(symbol) = self.symbols.lookup(name) {
            match &symbol.kind {
                crate::symbol::SymbolKind::Struct(_) => {
                    (symbol.name.clone(), symbol.module_path.clone())
                }
                _ => (name.clone(), Vec::new()),
            }
        } else {
            // Fall back to local struct (no module path)
            (name.clone(), Vec::new())
        };

        // Resolve field expressions first
        let fields: Vec<TirStructField> = struct_lit
            .fields
            .iter()
            .enumerate()
            .map(|(index, field)| {
                let value = self.resolve_expr(&field.value, ctx);
                TirStructField {
                    name: field.name.clone(),
                    value,
                    field_index: index as u32,
                }
            })
            .collect();

        // Check if this is a generic struct and infer type arguments
        let struct_type = if self.generic_struct_names.contains(&struct_name) {
            // This is a generic struct - infer type arguments from field values
            let mut type_args = self.infer_type_args_from_fields(&struct_name, &fields);
            // If we couldn't infer type from fields, try to use return type context
            // This handles cases like `return Array { repr: ..., used: 0 }` in generic functions
            if type_args.is_empty()
                && struct_name == "Array"
                && let Some(elem_type) = self.type_table.borrow().as_array(ctx.return_type)
            {
                type_args = vec![elem_type];
            }
            self.type_table.borrow_mut().make_generic_instance(
                struct_name.clone(),
                module_path,
                type_args,
            )
        } else {
            self.type_table
                .borrow_mut()
                .make_struct(struct_name.clone(), module_path)
        };

        TirExpr::new(
            TirExprKind::StructLiteral {
                struct_type,
                struct_name,
                fields,
            },
            struct_type,
            struct_lit.span,
        )
    }

    /// Infer type arguments for a generic struct from field values
    fn infer_type_args_from_fields(
        &self,
        struct_name: &str,
        fields: &[TirStructField],
    ) -> Vec<TypeId> {
        // Get the generic struct's field type information
        let Some((_, field_types)) = self.struct_fields.get(struct_name) else {
            return vec![];
        };

        // Build a map from type param TypeId to concrete TypeId
        let mut type_param_map: HashMap<TypeId, TypeId> = HashMap::new();

        for (struct_field, (_, expected_type_id)) in fields.iter().zip(field_types.iter()) {
            let actual_type_id = struct_field.value.type_id;

            // Check if expected type is a type parameter
            if let ResolvedType::TypeParam { .. } = self.type_table.borrow().get(*expected_type_id)
            {
                // Map this type param to the actual type
                type_param_map.insert(*expected_type_id, actual_type_id);
            }
        }

        // Collect type args in order (by TypeParam index)
        let mut type_args: Vec<(u32, TypeId)> = type_param_map
            .iter()
            .filter_map(|(&param_id, &concrete_id)| {
                if let ResolvedType::TypeParam { index, .. } =
                    self.type_table.borrow().get(param_id)
                {
                    Some((*index, concrete_id))
                } else {
                    None
                }
            })
            .collect();

        type_args.sort_by_key(|(index, _)| *index);
        type_args.into_iter().map(|(_, type_id)| type_id).collect()
    }

    /// Resolve a tuple literal expression: `[1, 2, 3]` or `[1, "hello", true]`
    fn resolve_tuple_literal(
        &mut self,
        tuple_lit: &ast::TupleLiteralExpr,
        ctx: &mut FunctionContext,
    ) -> TirExpr {
        // Resolve each element expression
        let elements: Vec<TirExpr> = tuple_lit
            .elements
            .iter()
            .map(|elem| self.resolve_expr(elem, ctx))
            .collect();

        // Collect element types for the tuple type
        let elem_types: Vec<TypeId> = elements.iter().map(|e| e.type_id).collect();
        let tuple_type = self.type_table.borrow_mut().make_tuple(elem_types);

        TirExpr::new(
            TirExprKind::TupleLiteral { elements },
            tuple_type,
            tuple_lit.span,
        )
    }

    /// Resolve a type from AST Type to TypeId
    fn resolve_type(&mut self, ty: &Type) -> TypeId {
        match ty {
            Type::Named(named) => self.resolve_named_type(&named.name, named.span),
            Type::Generic(generic) => {
                self.resolve_generic_type(&generic.name, &generic.args, generic.span)
            }
            Type::Function(func_ty) => {
                let params: Vec<TypeId> = func_ty
                    .params
                    .iter()
                    .map(|p| self.resolve_type(p))
                    .collect();
                let return_type = self.resolve_type(&func_ty.return_type);
                self.type_table.borrow_mut().make_function(
                    params,
                    return_type,
                    func_ty.effects.clone(),
                )
            }
            Type::Tuple(elements) => {
                let elem_types: Vec<TypeId> =
                    elements.iter().map(|e| self.resolve_type(e)).collect();
                self.type_table.borrow_mut().make_tuple(elem_types)
            }
            Type::Reference(inner) => {
                let inner_type = self.resolve_type(inner);
                self.type_table.borrow_mut().make_ref(inner_type)
            }
            Type::MutReference(inner) => {
                let inner_type = self.resolve_type(inner);
                self.type_table.borrow_mut().make_mut_ref(inner_type)
            }
            Type::NamespacedGeneric(namespaced) => self.resolve_namespaced_generic_type(namespaced),
        }
    }

    /// Resolve a namespaced generic type like `builtin::array<T>`
    fn resolve_namespaced_generic_type(
        &mut self,
        namespaced: &crate::ast::NamespacedGenericType,
    ) -> TypeId {
        match namespaced.namespace.as_str() {
            "builtin" => match namespaced.name.as_str() {
                "array" => {
                    if namespaced.args.len() != 1 {
                        self.errors.push(TypeError::ArgumentCountMismatch {
                            expected: 1,
                            found: namespaced.args.len(),
                            span: namespaced.span,
                        });
                        return TypeTable::ERROR;
                    }
                    let element_type = self.resolve_type(&namespaced.args[0]);
                    self.type_table
                        .borrow_mut()
                        .make_builtin_array(element_type)
                }
                _ => {
                    self.errors.push(TypeError::UnknownType {
                        name: format!("builtin::{}", namespaced.name),
                        span: namespaced.span,
                    });
                    TypeTable::ERROR
                }
            },
            _ => {
                self.errors.push(TypeError::UnknownType {
                    name: format!("{}::{}", namespaced.namespace, namespaced.name),
                    span: namespaced.span,
                });
                TypeTable::ERROR
            }
        }
    }

    /// Resolve a named type
    fn resolve_named_type(&mut self, name: &str, _span: Span) -> TypeId {
        // First check if it's a type parameter in scope
        if let Some(&(_, type_id)) = self.current_type_params.get(name) {
            return type_id;
        }

        match name {
            // Primitives
            "i8" => TypeTable::I8,
            "i16" => TypeTable::I16,
            "i32" => TypeTable::I32,
            "i64" => TypeTable::I64,
            "i128" => TypeTable::I128,
            "u8" => TypeTable::U8,
            "u16" => TypeTable::U16,
            "u32" => TypeTable::U32,
            "u64" => TypeTable::U64,
            "u128" => TypeTable::U128,
            "f32" => TypeTable::F32,
            "f64" => TypeTable::F64,
            "bool" => TypeTable::BOOL,
            "char" => TypeTable::CHAR,
            "()" => TypeTable::UNIT,
            "!" => TypeTable::NEVER,

            // Check type aliases, struct definitions, and variants
            _ => {
                if let Some(&type_id) = self.type_aliases.get(name) {
                    type_id
                } else if let Some((module_path, _)) = self.struct_fields.get(name) {
                    // It's a struct - use the module path where it was defined
                    self.type_table
                        .borrow_mut()
                        .make_struct(name.to_string(), module_path.clone())
                } else if let Some((module_path, _, _)) = self.variant_cases.get(name) {
                    // It's a variant - use the module path where it was defined
                    self.type_table
                        .borrow_mut()
                        .make_variant(name.to_string(), module_path.clone())
                } else {
                    // Unknown type
                    TypeTable::UNKNOWN
                }
            }
        }
    }

    /// Resolve a generic type
    fn resolve_generic_type(&mut self, name: &str, args: &[Type], span: Span) -> TypeId {
        // Prelude module path for looking up Option/Result
        let prelude_path = vec!["core".to_string(), "prelude".to_string()];

        match name {
            "Option" => {
                // Verify Option variant exists in symbol table (declared in prelude)
                // First check local imports, then fall back to prelude module
                let found_as_variant = self
                    .symbols
                    .lookup("Option")
                    .or_else(|| self.symbols.lookup_in_module(&prelude_path, "Option"))
                    .is_some_and(|s| matches!(s.kind, SymbolKind::Variant(_)));

                if !found_as_variant {
                    // Option not found as a variant - likely #![no_prelude] without explicit import
                    self.errors.push(TypeError::UnknownType {
                        name: "Option".to_string(),
                        span,
                    });
                }
                let inner = args
                    .first()
                    .map(|t| self.resolve_type(t))
                    .unwrap_or(TypeTable::UNKNOWN);
                self.type_table.borrow_mut().make_option(inner)
            }
            "Result" => {
                // Verify Result variant exists in symbol table (declared in prelude)
                // First check local imports, then fall back to prelude module
                let found_as_variant = self
                    .symbols
                    .lookup("Result")
                    .or_else(|| self.symbols.lookup_in_module(&prelude_path, "Result"))
                    .is_some_and(|s| matches!(s.kind, SymbolKind::Variant(_)));

                if !found_as_variant {
                    // Result not found as a variant - likely #![no_prelude] without explicit import
                    self.errors.push(TypeError::UnknownType {
                        name: "Result".to_string(),
                        span,
                    });
                }
                let ok = args
                    .first()
                    .map(|t| self.resolve_type(t))
                    .unwrap_or(TypeTable::UNKNOWN);
                let err = args
                    .get(1)
                    .map(|t| self.resolve_type(t))
                    .unwrap_or(TypeTable::UNKNOWN);
                self.type_table.borrow_mut().make_result(ok, err)
            }
            "Stream" => {
                let elem = args
                    .first()
                    .map(|t| self.resolve_type(t))
                    .unwrap_or(TypeTable::UNKNOWN);
                self.type_table
                    .borrow_mut()
                    .intern(ResolvedType::Stream(elem))
            }
            "Future" => {
                let elem = args
                    .first()
                    .map(|t| self.resolve_type(t))
                    .unwrap_or(TypeTable::UNKNOWN);
                self.type_table
                    .borrow_mut()
                    .intern(ResolvedType::Future(elem))
            }
            "Dict" => {
                let key = args
                    .first()
                    .map(|t| self.resolve_type(t))
                    .unwrap_or(TypeTable::UNKNOWN);
                let value = args
                    .get(1)
                    .map(|t| self.resolve_type(t))
                    .unwrap_or(TypeTable::UNKNOWN);
                self.type_table
                    .borrow_mut()
                    .intern(ResolvedType::Dict { key, value })
            }
            _ => {
                // Check if it's a user-defined generic struct
                if self.generic_struct_names.contains(name) {
                    // Resolve type arguments
                    let type_args: Vec<TypeId> =
                        args.iter().map(|t| self.resolve_type(t)).collect();

                    // Get the module path where the struct was defined
                    let module_path = self
                        .struct_fields
                        .get(name)
                        .map(|(path, _)| path.clone())
                        .unwrap_or_else(|| self.current_module_path.clone());

                    // Create a GenericInstance type
                    self.type_table.borrow_mut().make_generic_instance(
                        name.to_string(),
                        module_path,
                        type_args,
                    )
                } else {
                    TypeTable::UNKNOWN
                }
            }
        }
    }

    /// Get the type table (after resolution)
    pub fn into_type_table(self) -> Rc<RefCell<TypeTable>> {
        self.type_table
    }
}

/// Convert AST BinaryOp to TIR BinaryOp
fn convert_binary_op(op: BinaryOp) -> TirBinaryOp {
    match op {
        BinaryOp::Add => TirBinaryOp::Add,
        BinaryOp::Sub => TirBinaryOp::Sub,
        BinaryOp::Mul => TirBinaryOp::Mul,
        BinaryOp::Div => TirBinaryOp::Div,
        BinaryOp::Mod => TirBinaryOp::Mod,
        BinaryOp::Eq => TirBinaryOp::Eq,
        BinaryOp::NotEq => TirBinaryOp::NotEq,
        BinaryOp::Lt => TirBinaryOp::Lt,
        BinaryOp::LtEq => TirBinaryOp::LtEq,
        BinaryOp::Gt => TirBinaryOp::Gt,
        BinaryOp::GtEq => TirBinaryOp::GtEq,
        BinaryOp::And => TirBinaryOp::And,
        BinaryOp::Or => TirBinaryOp::Or,
        BinaryOp::BitAnd => TirBinaryOp::BitAnd,
        BinaryOp::BitOr => TirBinaryOp::BitOr,
        BinaryOp::BitXor => TirBinaryOp::BitXor,
        BinaryOp::Shl => TirBinaryOp::Shl,
        BinaryOp::Shr => TirBinaryOp::Shr,
    }
}

/// Convert AST UnaryOp to TIR UnaryOp
fn convert_unary_op(op: UnaryOp) -> TirUnaryOp {
    match op {
        UnaryOp::Neg => TirUnaryOp::Neg,
        UnaryOp::Not => TirUnaryOp::Not,
        UnaryOp::BitNot => TirUnaryOp::BitNot,
        UnaryOp::Ref => TirUnaryOp::Ref,
        UnaryOp::MutRef => TirUnaryOp::MutRef,
        UnaryOp::Deref => TirUnaryOp::Deref,
    }
}

/// Convenience function to resolve a module
pub fn resolve_module(
    module: &Module,
    module_path: Vec<String>,
    symbols: &SymbolTable,
    loaded_modules: &HashMap<Vec<String>, Module>,
) -> Result<TirModule, Vec<TypeError>> {
    let mut resolver = Resolver::new(symbols, loaded_modules);
    resolver.resolve_module(module, module_path)
}

/// Resolve all modules and return a Project ready for lowering.
///
/// This is the main entry point for the resolve phase. It resolves all modules
/// to TIR and packages them into a Project struct.
pub fn resolve_to_project(
    symbols: SymbolTable,
    modules: &HashMap<Vec<String>, Module>,
    entry_path: Vec<String>,
    implicit_modules: HashSet<Vec<String>>,
    module_name: String,
) -> Result<Project, Vec<TypeError>> {
    let tir_modules = Resolver::resolve_all_modules(&symbols, modules, &entry_path)?;

    Ok(Project::new(
        entry_path,
        tir_modules,
        symbols,
        implicit_modules,
        module_name,
    ))
}
