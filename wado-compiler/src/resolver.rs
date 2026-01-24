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

use crate::component_model::WasiRegistry;
use crate::name::{LocalMethodName, ModuleSource, strip_type_params};

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

/// Helper to get the `ModuleSource` for String type (core:prelude)
fn string_module_source() -> ModuleSource {
    ModuleSource::core("prelude")
}

/// Struct field info: module source and field definitions
#[derive(Clone)]
struct StructFieldInfo {
    module_source: ModuleSource,
    /// Field definitions: (name, `type_id`) pairs
    fields: Vec<(String, TypeId)>,
}

/// Variant case info: case name and field types
#[derive(Clone)]
struct VariantCaseData {
    name: String,
    field_types: Vec<TypeId>,
}

/// Variant info: module source, type parameters, and cases
#[derive(Clone)]
struct VariantInfo {
    module_source: ModuleSource,
    type_params: Vec<String>,
    cases: Vec<VariantCaseData>,
}

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

/// Labeled block expression target for tracking break types
#[derive(Debug, Clone)]
struct LabeledBlockTarget {
    /// The label name
    label: String,
    /// Types collected from `break label: expr;` statements
    break_types: Vec<TypeId>,
}

/// Function context during resolution with scope tracking
struct FunctionContext {
    /// Stack of scopes (each scope maps name -> `LocalVar`)
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
    /// Outer context locals for closure capture detection (name -> `LocalVar` snapshot)
    /// Only set for closure contexts
    outer_locals: HashMap<String, LocalVar>,
    /// Captured variables detected during resolution (name -> capture index)
    /// Only used for closure contexts
    captured_vars: HashMap<String, u32>,
    /// Stack of labeled block expression targets for tracking break types
    labeled_block_targets: Vec<LabeledBlockTarget>,
    /// Current function name for `#function` compile-time literal
    function_name: String,
}

impl FunctionContext {
    fn new(return_type: TypeId, function_name: String) -> Self {
        Self {
            scopes: vec![HashMap::new()], // Start with one scope for function parameters
            next_local: 0,
            return_type,
            local_types: Vec::new(),
            address_taken_locals: HashSet::new(),
            outer_locals: HashMap::new(),
            captured_vars: HashMap::new(),
            labeled_block_targets: Vec::new(),
            function_name,
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

        // Closure function name is parent::{closure}
        let function_name = format!("{}::{{closure}}", outer_ctx.function_name);

        Self {
            scopes: vec![HashMap::new()],
            next_local: 0,
            return_type,
            local_types: Vec::new(),
            address_taken_locals: HashSet::new(),
            outer_locals,
            captured_vars: HashMap::new(),
            labeled_block_targets: Vec::new(),
            function_name,
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

    /// Get the list of captures for building `TirCapture` entries
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
    /// Struct field info (struct name -> (`module_path`, fields))
    struct_fields: HashMap<String, StructFieldInfo>,
    /// Variant case info (variant name -> (`module_path`, `type_params`, cases))
    variant_cases: HashMap<String, VariantInfo>,
    /// Function return types (name -> return type)
    function_return_types: HashMap<String, TypeId>,
    /// Imported function names for the current module
    imported_functions: HashSet<String>,
    /// Errors collected during resolution
    errors: Vec<TypeError>,
    /// Current module source being resolved (for struct type `module_source`)
    current_module_source: ModuleSource,
    /// Current module items (for local function parameter lookup)
    current_module_items: Vec<Item>,
    /// Type parameters currently in scope (name -> (index, `TypeId`))
    /// Set when resolving generic structs or functions
    current_type_params: HashMap<String, (u32, TypeId)>,
    /// Generic struct definitions (name -> type param count)
    /// Used to determine if a struct is generic
    generic_struct_names: HashSet<String>,
    /// Generic function type parameters (`func_name` -> `type_params`)
    /// Used for substituting type parameters in return types
    generic_function_params: HashMap<String, Vec<(String, TypeId)>>,
    /// Generic method type parameters (`mangled_name` -> `type_params`)
    /// Used for substituting type parameters in method return types
    generic_method_params: HashMap<String, Vec<(String, TypeId)>>,
    /// Current associated type bindings in scope (`Self::Name` -> resolved type)
    /// Set when resolving trait implementations
    current_associated_type_bindings: HashMap<String, TypeId>,
    /// Current `Self` type in scope (the type being implemented in an impl block)
    current_self_type: Option<TypeId>,
    /// WASI registry for looking up effect return types
    wasi_registry: WasiRegistry,
}

/// Info about an Index trait implementation
struct IndexTraitInfo {
    /// The Output associated type
    output_type: TypeId,
    /// Self kind for the index method (&self)
    self_kind: ast::SelfKind,
    /// The trait name (e.g., "Index<i32>")
    trait_name: String,
}

/// Info about an `IndexAssign` trait implementation
struct IndexAssignTraitInfo {
    /// The Input associated type (reserved for future type checking)
    _input_type: TypeId,
    /// Self kind for the `index_assign` method (&mut self)
    self_kind: ast::SelfKind,
    /// The trait name (e.g., "`IndexAssign`<i32>")
    trait_name: String,
}

/// Info about an `IndexMut` trait implementation
struct IndexMutTraitInfo {
    /// The Output associated type
    output_type: TypeId,
    /// Self kind for the `index_mut` method (&mut self)
    self_kind: ast::SelfKind,
    /// The trait name (e.g., "`IndexMut`")
    trait_name: String,
}

/// Info about an `IndexValue` trait implementation
struct IndexValueTraitInfo {
    /// The Output associated type
    output_type: TypeId,
    /// Self kind for the `index_value` method (&self)
    self_kind: ast::SelfKind,
    /// The trait name (e.g., "`IndexValue`<i32>")
    trait_name: String,
}

/// Info about a comparison trait implementation (`Eq` or `Ord`)
struct ComparisonTraitInfo {
    /// Self kind for the comparison method (&self)
    self_kind: ast::SelfKind,
    /// The trait name (e.g., "Eq", "Ord")
    trait_name: String,
}

impl<'a> Resolver<'a> {
    /// Create a new resolver
    pub fn new(symbols: &'a SymbolTable, loaded_modules: &'a HashMap<Vec<String>, Module>) -> Self {
        let (wasi_registry, _) = WasiRegistry::build_from_stdlib();
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
            current_module_source: ModuleSource::entry_point(),
            current_module_items: Vec::new(),
            current_type_params: HashMap::new(),
            generic_struct_names: HashSet::new(),
            generic_function_params: HashMap::new(),
            generic_method_params: HashMap::new(),
            current_associated_type_bindings: HashMap::new(),
            current_self_type: None,
            wasi_registry,
        }
    }

    /// Resolve a module, converting AST to TIR
    pub fn resolve_module(
        &mut self,
        module: &Module,
        module_source: ModuleSource,
    ) -> Result<TirModule, Vec<TypeError>> {
        // Set current module source for struct type creation
        self.current_module_source = module_source.clone();
        // Store current module items for local function parameter lookup
        self.current_module_items = module.items.clone();

        // First pass: collect type definitions
        self.collect_types(module);

        // Second pass: collect function signatures (for call resolution)
        self.collect_function_signatures(module);

        // Third pass: resolve functions
        let mut tir_module = TirModule::new(module_source.clone());

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

                    // Register type parameters from impl block's generic type FIRST
                    // e.g., impl IndexValue<i32> for Triple<T> needs T registered
                    let old_type_params = std::mem::take(&mut self.current_type_params);
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

                    // Set up associated type bindings for trait implementations
                    // This now works because type params (like T) are registered above
                    let old_associated_type_bindings =
                        std::mem::take(&mut self.current_associated_type_bindings);
                    if impl_block.trait_type.is_some() {
                        for binding in &impl_block.associated_types {
                            let type_id = self.resolve_type(&binding.ty);
                            self.current_associated_type_bindings
                                .insert(binding.name.clone(), type_id);
                        }
                    }

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

                    // Restore old associated type bindings and type params
                    self.current_associated_type_bindings = old_associated_type_bindings;
                    self.current_type_params = old_type_params;
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
        entry_module_source: ModuleSource,
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
            // Use the provided entry_module_source for the entry module (empty path)
            // to preserve filename information
            let module_source = if path.is_empty() {
                entry_module_source.clone()
            } else {
                ModuleSource::from_path(path)
            };
            for item in &module.items {
                match item {
                    Item::Struct(struct_decl) => {
                        // Insert with empty fields first - will be populated in second sub-pass
                        struct_fields.insert(
                            struct_decl.name.clone(),
                            StructFieldInfo {
                                module_source: module_source.clone(),
                                fields: Vec::new(),
                            },
                        );
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
                            VariantInfo {
                                module_source: module_source.clone(),
                                type_params,
                                cases: Vec::new(),
                            },
                        );
                    }
                    _ => {}
                }
            }
        }

        // Second sub-pass: resolve struct fields and type aliases
        for (path, module) in modules {
            // Use the provided entry_module_source for the entry module (empty path)
            let module_source = if path.is_empty() {
                entry_module_source.clone()
            } else {
                ModuleSource::from_path(path)
            };
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
                        struct_fields.insert(
                            struct_decl.name.clone(),
                            StructFieldInfo {
                                module_source: module_source.clone(),
                                fields,
                            },
                        );
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
                            cases.push(VariantCaseData {
                                name: case.name.clone(),
                                field_types,
                            });
                        }
                        variant_cases.insert(
                            variant_decl.name.clone(),
                            VariantInfo {
                                module_source: module_source.clone(),
                                type_params,
                                cases,
                            },
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

            let (wasi_registry, _) = WasiRegistry::build_from_stdlib();
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
                current_module_source: ModuleSource::entry_point(), // Set in resolve_module
                current_module_items: Vec::new(),                   // Set in resolve_module
                current_type_params: HashMap::new(),
                generic_struct_names: HashSet::new(),
                generic_function_params: HashMap::new(),
                generic_method_params: HashMap::new(),
                current_associated_type_bindings: HashMap::new(),
                current_self_type: None,
                wasi_registry,
            };

            // Use the provided entry_module_source for the entry module (empty path)
            // to preserve filename information
            let module_source = if path.is_empty() {
                entry_module_source.clone()
            } else {
                ModuleSource::from_path(path)
            };
            match resolver.resolve_module(module, module_source) {
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
        for (struct_name, info) in struct_fields {
            let module_path = info.module_source.to_path();
            let Some(&from_idx) = path_to_idx.get(&module_path) else {
                continue;
            };
            for (_field_name, field_type_id) in &info.fields {
                if let ResolvedType::Struct {
                    name: ref_struct_name,
                    module_source: ref_module_source,
                } = type_table.get(*field_type_id)
                {
                    // Skip self-references (same struct or same module)
                    let ref_module_path = ref_module_source.to_path();
                    if ref_struct_name == struct_name || ref_module_path == module_path {
                        continue;
                    }
                    if let Some(&to_idx) = path_to_idx.get(&ref_module_path) {
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

    /// Static version of `resolve_type` for use before the resolver is fully constructed
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
                        if let Some(info) = struct_fields.get(&named.name) {
                            type_table.make_struct(named.name.clone(), info.module_source.clone())
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
                    if let Some(info) = struct_fields.get(&generic.name) {
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
                            info.module_source.clone(),
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
                        StructFieldInfo {
                            module_source: self.current_module_source.clone(),
                            fields,
                        },
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
                        cases.push(VariantCaseData {
                            name: case.name.clone(),
                            field_types,
                        });
                    }

                    self.variant_cases.insert(
                        variant_decl.name.clone(),
                        VariantInfo {
                            module_source: self.current_module_source.clone(),
                            type_params,
                            cases,
                        },
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

                    // Set up associated type bindings for trait implementations
                    let old_associated_type_bindings =
                        std::mem::take(&mut self.current_associated_type_bindings);
                    if impl_block.trait_type.is_some() {
                        for binding in &impl_block.associated_types {
                            let type_id = self.resolve_type(&binding.ty);
                            self.current_associated_type_bindings
                                .insert(binding.name.clone(), type_id);
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

                    // Restore type parameters and associated type bindings
                    self.current_type_params = old_type_params;
                    self.current_associated_type_bindings = old_associated_type_bindings;
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

        let mut ctx = FunctionContext::new(return_type, func.name.clone());

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
            method_info: None,        // Not a method
            params,
            return_type,
            effects: func.effects.clone(),
            body,
            span: func.span,
            local_count: ctx.next_local,
            local_types: ctx.local_types,
            address_taken_locals: ctx.address_taken_locals,
            needed_copy_types: std::collections::HashSet::new(),
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

        // Set up Self type for the impl block
        // This allows `&Self` to resolve correctly in method parameters
        let old_self_type = self.current_self_type;
        self.current_self_type = Some(self.resolve_type(impl_type));

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

        // Display name for #function: StructName::method_name
        let display_name = format!("{}::{}", struct_name, func.name);
        let mut ctx = FunctionContext::new(return_type, display_name);

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

        // Restore Self type
        self.current_self_type = old_self_type;

        Some(TirFunction {
            name: func.name.clone(), // Will be mangled by caller
            is_pub: func.is_pub,
            type_params,
            impl_type_params, // Type params from impl block (e.g., T from impl Counter<T>)
            monomorph_info: None, // Not from monomorphization
            method_info: Some(LocalMethodName {
                struct_name: struct_name.to_string(),
                trait_name: trait_name.map(String::from),
                method_name: func.name.clone(),
                method_type_args: vec![],
            }),
            params,
            return_type,
            effects: func.effects.clone(),
            body,
            span: func.span,
            local_count: ctx.next_local,
            local_types: ctx.local_types,
            address_taken_locals: ctx.address_taken_locals,
            needed_copy_types: std::collections::HashSet::new(),
        })
    }

    /// Get the type name from a Type node
    fn get_type_name(&self, ty: &Type) -> String {
        match ty {
            Type::Named(named) => named.name.clone(),
            Type::Generic(generic) => generic.name.clone(),
            Type::Reference(inner) => self.get_type_name(inner),
            Type::MutReference(inner) => self.get_type_name(inner),
            Type::Function(func_type) => {
                // Build function type string: "fn(T1, T2) -> R"
                let param_strs: Vec<String> = func_type
                    .params
                    .iter()
                    .map(|p| self.get_type_name(p))
                    .collect();
                let return_str = self.get_type_name(&func_type.return_type);
                format!("fn({}) -> {}", param_strs.join(", "), return_str)
            }
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
            Stmt::Break(break_stmt) => vec![self.resolve_break(break_stmt, ctx)],
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
                        module_source: _,
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

    /// Resolve a for statement - generates a For node with init statements included
    /// The For node handles continue correctly (executes update before next iteration)
    /// The init variable is scoped to the for loop and not visible after it
    fn resolve_for(&mut self, for_stmt: &ForStmt, ctx: &mut FunctionContext) -> Vec<TirStmt> {
        // Enter scope for the for loop's init variable
        ctx.enter_scope();

        // Resolve init statement if present (e.g., let i = 0)
        let init = if let Some(init_stmt) = &for_stmt.init {
            self.resolve_stmt(init_stmt, ctx)
        } else {
            Vec::new()
        };

        // Resolve the body (note: resolve_block enters its own scope for body variables)
        let body = self.resolve_block(&for_stmt.body, ctx);

        // Resolve condition (None means infinite loop)
        let condition = for_stmt
            .condition
            .as_ref()
            .map(|c| self.resolve_expr(c, ctx));

        // Resolve update expression
        let update = for_stmt.update.as_ref().map(|u| self.resolve_expr(u, ctx));

        // Create For statement with init included
        let for_tir = TirStmt::new(
            TirStmtKind::For {
                init,
                condition,
                body,
                update,
            },
            for_stmt.span,
        );

        // Exit the for loop's scope
        ctx.exit_scope();

        vec![for_tir]
    }

    /// Resolve a for-of statement: `for let item of iterable { ... }`
    ///
    /// For Arrays, uses the optimized `TirStmtKind::ForOf` that generates efficient WASM.
    /// For other types implementing `IntoIterator`, desugars to:
    /// ```text
    /// {
    ///     let mut __iter = iterable.into_iter();
    ///     loop {
    ///         if let Some(binding) = __iter.next() {
    ///             body
    ///         } else {
    ///             break;
    ///         }
    ///     }
    /// }
    /// ```
    fn resolve_for_of(&mut self, for_of_stmt: &ForOfStmt, ctx: &mut FunctionContext) -> TirStmt {
        // Resolve the iterable expression
        let iterable = self.resolve_expr(&for_of_stmt.iterable, ctx);
        let iterable_type = iterable.type_id;

        // Fast path: Array<T> - use optimized codegen
        let element_type_opt = self.type_table.borrow().as_array(iterable_type);
        if let Some(element_type) = element_type_opt {
            return self.resolve_for_of_array(for_of_stmt, iterable, element_type, ctx);
        }

        // Slow path: Check for IntoIterator implementation
        if let Some((item_type, iter_type)) = self.find_into_iterator_impl(iterable_type) {
            return self.resolve_for_of_into_iterator(
                for_of_stmt,
                iterable,
                iterable_type,
                item_type,
                iter_type,
                ctx,
            );
        }

        // No valid iteration method found
        self.errors.push(TypeError::TypeMismatch {
            expected: "Array<T> or type implementing IntoIterator".to_string(),
            found: self.type_table.borrow().type_name(iterable_type),
            span: for_of_stmt.iterable.span(),
        });

        // Generate a dummy loop to avoid cascading errors
        ctx.enter_scope();
        let binding_local = ctx.add_local(
            for_of_stmt.binding.clone(),
            TypeTable::UNKNOWN,
            for_of_stmt.is_mut,
        );
        let body = self.resolve_block(&for_of_stmt.body, ctx);
        ctx.exit_scope();

        TirStmt::new(
            TirStmtKind::ForOf {
                binding_local,
                binding_type: TypeTable::UNKNOWN,
                is_mut: for_of_stmt.is_mut,
                iterable,
                iterable_type,
                body,
            },
            for_of_stmt.span,
        )
    }

    /// Optimized for-of for Array<T> - generates direct array access in codegen
    fn resolve_for_of_array(
        &mut self,
        for_of_stmt: &ForOfStmt,
        iterable: TirExpr,
        element_type: TypeId,
        ctx: &mut FunctionContext,
    ) -> TirStmt {
        let iterable_type = iterable.type_id;

        ctx.enter_scope();
        let binding_local = ctx.add_local(
            for_of_stmt.binding.clone(),
            element_type,
            for_of_stmt.is_mut,
        );
        let body = self.resolve_block(&for_of_stmt.body, ctx);
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

    /// Desugar for-of for `IntoIterator` types into a loop with iterator method calls
    fn resolve_for_of_into_iterator(
        &mut self,
        for_of_stmt: &ForOfStmt,
        iterable: TirExpr,
        iterable_type: TypeId,
        item_type: TypeId,
        iter_type: TypeId,
        ctx: &mut FunctionContext,
    ) -> TirStmt {
        let span = for_of_stmt.span;

        // Create outer scope for the whole desugared construct
        ctx.enter_scope();

        // Generate: let mut __iter = iterable.into_iter();
        let iter_local = ctx.add_local("__iter".to_string(), iter_type, true);

        // Get base type info for method name mangling
        let (struct_name, _module_path, type_args) = self.get_type_info_for_method(iterable_type);

        // Build into_iter() method call
        // Receiver needs to be a reference: &iterable
        let ref_type = self.type_table.borrow_mut().make_ref(iterable_type);
        let receiver = TirExpr::new(
            TirExprKind::Unary {
                op: TirUnaryOp::Ref,
                expr: Box::new(iterable),
            },
            ref_type,
            span,
        );

        let into_iter_method_name = format!("{struct_name}^IntoIterator::into_iter");
        let into_iter_call = TirExpr::new(
            TirExprKind::MethodCall {
                receiver: Box::new(receiver),
                func: FunctionRef::External {
                    module_source: ModuleSource::core("prelude"),
                    name: into_iter_method_name,
                    monomorph_info: None,
                    method_info: Some(crate::name::LocalMethodName::new(
                        struct_name.clone(),
                        Some("IntoIterator".to_string()),
                        "into_iter".to_string(),
                    )),
                },
                type_args: type_args.clone().unwrap_or_default(),
                args: vec![],
            },
            iter_type,
            span,
        );

        let let_iter_stmt = TirStmt::new(
            TirStmtKind::Let {
                name: "__iter".to_string(),
                local_index: iter_local,
                is_mut: true,
                is_reactive: false,
                type_id: iter_type,
                value: into_iter_call,
            },
            span,
        );

        // Create inner scope for the loop body
        ctx.enter_scope();

        // Add the user's binding variable
        let binding_local =
            ctx.add_local(for_of_stmt.binding.clone(), item_type, for_of_stmt.is_mut);

        // Resolve the user's body
        let user_body = self.resolve_block(&for_of_stmt.body, ctx);

        ctx.exit_scope();

        // Get iterator type info for next() method name
        let (iter_struct_name, _iter_module_path, iter_type_args) =
            self.get_type_info_for_method(iter_type);

        // Build __iter.next() call
        // Receiver needs to be a mutable reference: &mut __iter
        let iter_local_expr = TirExpr::new(
            TirExprKind::Local {
                index: iter_local,
                name: "__iter".to_string(),
            },
            iter_type,
            span,
        );
        let mut_ref_type = self.type_table.borrow_mut().make_mut_ref(iter_type);
        let next_receiver = TirExpr::new(
            TirExprKind::Unary {
                op: TirUnaryOp::MutRef,
                expr: Box::new(iter_local_expr),
            },
            mut_ref_type,
            span,
        );

        let next_method_name = format!("{iter_struct_name}^Iterator::next");
        let option_item_type = self
            .type_table
            .borrow_mut()
            .intern(ResolvedType::Option(item_type));

        let next_call = TirExpr::new(
            TirExprKind::MethodCall {
                receiver: Box::new(next_receiver),
                func: FunctionRef::External {
                    module_source: ModuleSource::core("prelude"),
                    name: next_method_name,
                    monomorph_info: None,
                    method_info: Some(crate::name::LocalMethodName::new(
                        iter_struct_name,
                        Some("Iterator".to_string()),
                        "next".to_string(),
                    )),
                },
                type_args: iter_type_args.unwrap_or_default(),
                args: vec![],
            },
            option_item_type,
            span,
        );

        // Build: if let Some(binding) = __iter.next() { body } else { break; }
        let some_pattern = TirPattern::Variant {
            variant_name: "Some".to_string(),
            enum_type: option_item_type,
            bindings: vec![TirPattern::Binding {
                name: for_of_stmt.binding.clone(),
                local_index: binding_local,
            }],
        };

        let break_stmt = TirStmt::new(
            TirStmtKind::Break {
                label: None,
                value: None,
            },
            span,
        );

        let if_pattern = TirStmt::new(
            TirStmtKind::IfPattern {
                scrutinee: next_call,
                pattern: some_pattern,
                then_block: user_body,
                else_block: Some(TirBlock::new(vec![break_stmt], span)),
            },
            span,
        );

        // Build: loop { if let Some(binding) = __iter.next() { body } else { break; } }
        let loop_stmt = TirStmt::new(
            TirStmtKind::Loop {
                body: TirBlock::new(vec![if_pattern], span),
            },
            span,
        );

        ctx.exit_scope();

        // Wrap everything in a labeled block to create proper scope
        TirStmt::new(
            TirStmtKind::LabeledBlock {
                label: "__for_of".to_string(),
                block: TirBlock::new(vec![let_iter_stmt, loop_stmt], span),
            },
            span,
        )
    }

    /// Find `IntoIterator` implementation for a type and return (Item, Iter) types
    fn find_into_iterator_impl(&mut self, type_id: TypeId) -> Option<(TypeId, TypeId)> {
        // Get base type for lookup
        let base_type_id = self.get_base_type(type_id);
        let base_type = self.type_table.borrow().get(base_type_id).clone();

        let (struct_name, module_path, type_args) = match &base_type {
            ResolvedType::Struct {
                name,
                module_source,
            } => (name.clone(), module_source.to_path(), None),
            ResolvedType::GenericInstance {
                name,
                module_source,
                type_args,
            } => (
                name.clone(),
                module_source.to_path(),
                if type_args.is_empty() {
                    None
                } else {
                    Some(type_args.clone())
                },
            ),
            _ => return None,
        };

        // Look for impl IntoIterator for StructName
        self.find_into_iterator_impl_for_struct(&struct_name, &module_path, type_args.as_deref())
    }

    /// Find `IntoIterator` impl for a struct and return (Item, Iter) associated types
    fn find_into_iterator_impl_for_struct(
        &mut self,
        struct_name: &str,
        module_path: &[String],
        receiver_type_args: Option<&[TypeId]>,
    ) -> Option<(TypeId, TypeId)> {
        // Collect impl blocks from all modules
        let mut impl_blocks_to_check: Vec<(Type, Type, Vec<crate::ast::AssociatedTypeBinding>)> =
            Vec::new();

        // Check specific module if provided
        if !module_path.is_empty()
            && let Some(module) = self.loaded_modules.get(module_path)
        {
            for item in &module.items {
                if let Item::Impl(impl_block) = item
                    && let Some(trait_type) = &impl_block.trait_type
                    && self.get_type_name(trait_type) == "IntoIterator"
                {
                    impl_blocks_to_check.push((
                        impl_block.ty.clone(),
                        trait_type.clone(),
                        impl_block.associated_types.clone(),
                    ));
                }
            }
        }

        // Also check all loaded modules
        for module in self.loaded_modules.values() {
            for item in &module.items {
                if let Item::Impl(impl_block) = item
                    && let Some(trait_type) = &impl_block.trait_type
                    && self.get_type_name(trait_type) == "IntoIterator"
                {
                    impl_blocks_to_check.push((
                        impl_block.ty.clone(),
                        trait_type.clone(),
                        impl_block.associated_types.clone(),
                    ));
                }
            }
        }

        // Check current module items
        for item in &self.current_module_items {
            if let Item::Impl(impl_block) = item
                && let Some(trait_type) = &impl_block.trait_type
                && self.get_type_name(trait_type) == "IntoIterator"
            {
                impl_blocks_to_check.push((
                    impl_block.ty.clone(),
                    trait_type.clone(),
                    impl_block.associated_types.clone(),
                ));
            }
        }

        // Find matching impl
        for (impl_ty, _trait_type, associated_types) in impl_blocks_to_check {
            let impl_struct_name = self.get_type_name(&impl_ty);
            if impl_struct_name == struct_name {
                // Set up type params for generic impls
                let old_type_params = std::mem::take(&mut self.current_type_params);
                if let Some(type_args) = receiver_type_args
                    && let Type::Generic(generic) = &impl_ty
                {
                    for (i, arg) in generic.args.iter().enumerate() {
                        if let Type::Named(named) = arg
                            && i < type_args.len()
                        {
                            self.current_type_params
                                .insert(named.name.clone(), (i as u32, type_args[i]));
                        }
                    }
                }

                // Resolve associated types
                let mut item_type = TypeTable::UNKNOWN;
                let mut iter_type = TypeTable::UNKNOWN;

                for binding in &associated_types {
                    let resolved = self.resolve_type(&binding.ty);
                    match binding.name.as_str() {
                        "Item" => item_type = resolved,
                        "Iter" => iter_type = resolved,
                        _ => {}
                    }
                }

                self.current_type_params = old_type_params;

                if item_type != TypeTable::UNKNOWN && iter_type != TypeTable::UNKNOWN {
                    return Some((item_type, iter_type));
                }
            }
        }

        None
    }

    /// Get type info for method call generation: (`struct_name`, `module_path`, `type_args`)
    fn get_type_info_for_method(
        &self,
        type_id: TypeId,
    ) -> (String, Vec<String>, Option<Vec<TypeId>>) {
        let base_type_id = self.get_base_type(type_id);
        match self.type_table.borrow().get(base_type_id).clone() {
            ResolvedType::Struct {
                name,
                module_source,
            } => (name, module_source.to_path(), None),
            ResolvedType::GenericInstance {
                name,
                module_source,
                type_args,
            } => (
                name,
                module_source.to_path(),
                if type_args.is_empty() {
                    None
                } else {
                    Some(type_args)
                },
            ),
            ResolvedType::BuiltinArray(elem) => (
                "Array".to_string(),
                vec!["core".to_string(), "prelude".to_string()],
                Some(vec![elem]),
            ),
            _ => ("unknown".to_string(), vec![], None),
        }
    }

    /// Resolve a loop statement (infinite loop)
    fn resolve_loop(&mut self, loop_stmt: &LoopStmt, ctx: &mut FunctionContext) -> TirStmt {
        let body = self.resolve_block(&loop_stmt.body, ctx);
        TirStmt::new(TirStmtKind::Loop { body }, loop_stmt.span)
    }

    /// Resolve a break statement
    fn resolve_break(&mut self, break_stmt: &BreakStmt, ctx: &mut FunctionContext) -> TirStmt {
        let value = break_stmt.value.as_ref().map(|v| self.resolve_expr(v, ctx));

        // If breaking with a value to a labeled block expression, record the type
        if let (Some(label), Some(val)) = (&break_stmt.label, &value) {
            // Find the labeled block target with this label
            for target in &mut ctx.labeled_block_targets {
                if &target.label == label {
                    target.break_types.push(val.type_id);
                    break;
                }
            }
        }

        TirStmt::new(
            TirStmtKind::Break {
                label: break_stmt.label.clone(),
                value,
            },
            break_stmt.span,
        )
    }

    /// Resolve a continue statement
    fn resolve_continue(&mut self, continue_stmt: &ContinueStmt) -> TirStmt {
        TirStmt::new(TirStmtKind::Continue, continue_stmt.span)
    }

    /// Resolve an expression
    fn resolve_expr(&mut self, expr: &Expr, ctx: &mut FunctionContext) -> TirExpr {
        match expr {
            Expr::Literal(lit) => self.resolve_literal(lit, ctx),
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
            Expr::LabeledBlock(lb) => {
                // Labeled block expression: type is determined by `break label: expr;` statements
                // Push a target to track break types
                ctx.labeled_block_targets.push(LabeledBlockTarget {
                    label: lb.label.clone(),
                    break_types: Vec::new(),
                });

                ctx.enter_scope();
                let tir_block = self.resolve_block(&lb.block, ctx);
                ctx.exit_scope();

                // Pop the target and determine the result type from break statements
                let target = ctx.labeled_block_targets.pop().unwrap();

                // The result type is determined by the break expressions
                // All break values must have the same type (or be unifiable)
                let result_type = if target.break_types.is_empty() {
                    // No break with value - block produces Unit
                    TypeTable::UNIT
                } else {
                    // Use the first break type (TODO: type unification for multiple breaks)
                    target.break_types[0]
                };

                TirExpr::new(
                    TirExprKind::LabeledBlock {
                        label: lb.label.clone(),
                        block: tir_block,
                        result_type,
                    },
                    result_type,
                    lb.span,
                )
            }
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
    fn resolve_literal(&mut self, lit: &ast::LiteralExpr, ctx: &FunctionContext) -> TirExpr {
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
            Literal::LocationFile => {
                // #file - returns the current module source as a string
                let file_path = self.current_module_source.to_string();
                let string_type = self.get_string_struct_type();
                (TirExprKind::StringLiteral(file_path), string_type)
            }
            Literal::LocationLine => {
                // #line - returns the line number (1-indexed)
                let line = lit.span.line as u64;
                (
                    TirExprKind::IntLiteral {
                        value: line,
                        repr: line.to_string(),
                    },
                    TypeTable::I32,
                )
            }
            Literal::LocationFunction => {
                // #function - returns the current function name
                let string_type = self.get_string_struct_type();
                (
                    TirExprKind::StringLiteral(ctx.function_name.clone()),
                    string_type,
                )
            }
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

            if let Some(variant_info) = self.variant_cases.get(prefix) {
                // Find the case by name
                if let Some((case_index, case_data)) = variant_info
                    .cases
                    .iter()
                    .enumerate()
                    .find(|(_, c)| c.name == suffix)
                {
                    // Unit variant - must have no fields
                    if !case_data.field_types.is_empty() {
                        self.errors.push(TypeError::ArgumentCountMismatch {
                            expected: case_data.field_types.len(),
                            found: 0,
                            span: ident.span,
                        });
                        return TirExpr::new(TirExprKind::Unit, TypeTable::ERROR, ident.span);
                    }

                    // Create variant type
                    let variant_type = self
                        .type_table
                        .borrow_mut()
                        .make_variant(prefix.to_string(), variant_info.module_source.clone());

                    return TirExpr::new(
                        TirExprKind::VariantConstruct {
                            variant_type,
                            case_index: case_index as u32,
                            case_name: case_data.name.clone(),
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
                module_source: ModuleSource::entry_point(),
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

        // Check if this is a comparison operation on a non-primitive type
        // Non-primitives use Eq/Ord traits instead of direct Wasm instructions
        let left_type = self.type_table.borrow().get(left.type_id).clone();
        let is_comparison = matches!(
            binary.op,
            BinaryOp::Eq
                | BinaryOp::NotEq
                | BinaryOp::Lt
                | BinaryOp::LtEq
                | BinaryOp::Gt
                | BinaryOp::GtEq
        );

        if is_comparison {
            // Get struct name for trait lookup
            let struct_name = match &left_type {
                ResolvedType::Struct { name, .. } => Some(name.clone()),
                ResolvedType::GenericInstance { name, .. } => Some(name.clone()),
                _ => None,
            };

            if let Some(struct_name) = struct_name {
                // Determine which trait and method to use based on operator
                let (trait_name, method_name, needs_negation, swap_operands) = match binary.op {
                    BinaryOp::Eq => ("Eq", "eq", false, false),
                    BinaryOp::NotEq => ("Eq", "eq", true, false), // !a.eq(&b)
                    BinaryOp::Lt => ("Ord", "lt", false, false),
                    BinaryOp::Gt => ("Ord", "lt", false, true), // b.lt(&a)
                    BinaryOp::LtEq => ("Ord", "lt", true, true), // !b.lt(&a)
                    BinaryOp::GtEq => ("Ord", "lt", true, false), // !a.lt(&b)
                    _ => unreachable!(),
                };

                // Find the appropriate trait implementation
                let trait_info = if trait_name == "Eq" {
                    self.find_eq_trait_impl(&struct_name, left.type_id)
                } else {
                    self.find_ord_trait_impl(&struct_name, left.type_id)
                };

                if let Some(trait_info) = trait_info {
                    // Choose receiver and argument based on operand order
                    let (receiver_expr, arg_expr) = if swap_operands {
                        (right.clone(), left.clone())
                    } else {
                        (left.clone(), right.clone())
                    };

                    // Adjust receiver for self kind (&self)
                    let receiver = self.adjust_receiver_for_self_kind(
                        receiver_expr,
                        trait_info.self_kind,
                        binary.span,
                    );

                    // Create reference type for the argument (other: &Self)
                    let arg_ref_type = self
                        .type_table
                        .borrow_mut()
                        .intern(ResolvedType::Ref(arg_expr.type_id));

                    let arg_ref = TirExpr::new(
                        TirExprKind::Unary {
                            op: TirUnaryOp::Ref,
                            expr: Box::new(arg_expr),
                        },
                        arg_ref_type,
                        binary.span,
                    );

                    // Get the mangled method name: StructName^Eq::eq or StructName^Ord::lt
                    let mangled_method_name =
                        format!("{}^{}::{}", struct_name, trait_info.trait_name, method_name);

                    let call_expr = TirExpr::new(
                        TirExprKind::MethodCall {
                            receiver: Box::new(receiver),
                            func: FunctionRef::External {
                                module_source: ModuleSource::core("prelude"),
                                name: mangled_method_name,
                                monomorph_info: None,
                                method_info: Some(LocalMethodName::new(
                                    struct_name.clone(),
                                    Some(trait_info.trait_name.clone()),
                                    method_name.to_string(),
                                )),
                            },
                            type_args: vec![],
                            args: vec![arg_ref],
                        },
                        TypeTable::BOOL,
                        binary.span,
                    );

                    // Apply negation if needed (for !=, <=, >=)
                    if needs_negation {
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
            }
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
                found: format!("immutable variable '{name}'"),
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
        // Check for index assignment on custom types: arr[i] = value -> arr.index_assign(i, value)
        if let ast::Expr::Index(index_expr) = &assign.target {
            // Resolve the indexed expression to get its type
            let indexed_expr = self.resolve_expr(&index_expr.expr, ctx);

            // Get base type (unwrap reference if needed)
            let base_type_id = match self.type_table.borrow().get(indexed_expr.type_id) {
                ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => *inner,
                _ => indexed_expr.type_id,
            };

            // Check for IndexAssign trait implementation
            // Arrays now use IndexAssign trait like other types
            {
                // Check for IndexAssign trait implementation
                let struct_name = match self.type_table.borrow().get(base_type_id).clone() {
                    ResolvedType::Struct { name, .. } => name,
                    ResolvedType::GenericInstance { name, .. } => name,
                    _ => String::new(),
                };

                if !struct_name.is_empty() {
                    let index_resolved = self.resolve_expr(&index_expr.index, ctx);
                    let index_type = index_resolved.type_id;

                    if let Some(trait_info) =
                        self.find_index_assign_trait_impl(&struct_name, base_type_id, index_type)
                    {
                        // Generate: expr.index_assign(index, value)
                        let value = self.resolve_expr(&assign.value, ctx);

                        let receiver = self.adjust_receiver_for_self_kind(
                            indexed_expr,
                            trait_info.self_kind,
                            assign.span,
                        );

                        // Get the mangled method name: StructName^IndexAssign<IndexType>::index_assign
                        let mangled_method_name =
                            format!("{}^{}::index_assign", struct_name, trait_info.trait_name);

                        return TirExpr::new(
                            TirExprKind::MethodCall {
                                receiver: Box::new(receiver),
                                func: FunctionRef::External {
                                    module_source: self.current_module_source.clone(),
                                    name: mangled_method_name,
                                    monomorph_info: None,
                                    method_info: Some(LocalMethodName::new(
                                        struct_name.clone(),
                                        Some(trait_info.trait_name.clone()),
                                        "index_assign".to_string(),
                                    )),
                                },
                                type_args: vec![],
                                args: vec![index_resolved, value],
                            },
                            TypeTable::UNIT,
                            assign.span,
                        );
                    }
                }
            }
        }

        // Standard assignment handling
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

        // Check if this is a field access to a function-typed field (e.g., (self.f)(arg))
        // This handles calling closures stored in struct fields
        if let Expr::FieldAccess(_field_access) = &call.callee {
            // Resolve the callee expression to get the field type
            let callee_expr = self.resolve_expr(&call.callee, ctx);
            let callee_type = self.type_table.borrow().get(callee_expr.type_id).clone();

            if let ResolvedType::Function {
                params: fn_params,
                return_type,
                ..
            } = callee_type
            {
                // This is calling a function stored in a field!
                let fn_return_type = return_type;
                let fn_params = fn_params.clone();

                // Resolve arguments with coercion awareness based on function param types
                let args: Vec<TirExpr> = call
                    .args
                    .iter()
                    .enumerate()
                    .map(|(i, arg)| {
                        let expected_type = fn_params.get(i).copied();
                        self.resolve_expr_with_expected_type(arg, ctx, expected_type)
                    })
                    .collect();

                return TirExpr::new(
                    TirExprKind::IndirectCall {
                        callee: Box::new(callee_expr),
                        args,
                    },
                    fn_return_type,
                    call.span,
                );
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
                        let mangled_name = format!("{prefix}::{suffix}");
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
                    else if let Some(variant_info) = self.variant_cases.get(prefix) {
                        // Find the case by name
                        if let Some((case_index, case_data)) = variant_info
                            .cases
                            .iter()
                            .enumerate()
                            .find(|(_, c)| c.name == suffix)
                        {
                            // Validate argument count
                            if args.len() != case_data.field_types.len() {
                                self.errors.push(TypeError::ArgumentCountMismatch {
                                    expected: case_data.field_types.len(),
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
                            let variant_type = self.type_table.borrow_mut().make_variant(
                                prefix.to_string(),
                                variant_info.module_source.clone(),
                            );

                            return TirExpr::new(
                                TirExprKind::VariantConstruct {
                                    variant_type,
                                    case_index: case_index as u32,
                                    case_name: case_data.name.clone(),
                                    fields: args,
                                },
                                variant_type,
                                call.span,
                            );
                        } else {
                            // Unknown case name
                            self.errors.push(TypeError::UnknownFunction {
                                name: format!("{prefix}::{suffix}"),
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
                    module_source: ModuleSource::from_path(&module_path),
                    name: func_name,
                    monomorph_info: None,
                    method_info: None, // Free function call
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
            // Clone the return type AST and type params to avoid borrow issues
            let func_info = self.loaded_modules.get(module_path).and_then(|module| {
                module.items.iter().find_map(|item| {
                    if let Item::Function(func) = item
                        && func.name == func_name
                    {
                        Some((func.return_type.clone(), func.type_params.clone()))
                    } else {
                        None
                    }
                })
            });

            if let Some((ty, type_params)) = func_info
                && let Some(return_type_ast) = ty
            {
                // Set up the function's type parameters in scope so we can resolve
                // type parameter references (like T -> TypeParam { index: 0 })
                let old_type_params = std::mem::take(&mut self.current_type_params);
                for (i, type_param) in type_params.iter().enumerate() {
                    let type_id = self
                        .type_table
                        .borrow_mut()
                        .make_type_param(type_param.name.clone(), i as u32);
                    self.current_type_params
                        .insert(type_param.name.clone(), (i as u32, type_id));
                }

                let resolved = self.resolve_type(&return_type_ast);

                // Restore previous type params
                self.current_type_params = old_type_params;

                return resolved;
            }
        }

        // Default to UNIT for unknown functions (they might be external/builtin)
        TypeTable::UNIT
    }

    /// Get the return type of a WASI effect operation from the registry
    fn get_wasi_effect_return_type(&mut self, effect: &str, operation: &str) -> Option<TypeId> {
        // Look up the function in the WASI registry and clone the return type
        // to avoid borrow checker issues
        let func_key = format!("{effect}::{operation}");
        let return_type = self
            .wasi_registry
            .get_function(&func_key)?
            .return_type
            .clone()?;

        // Resolve the AST type to a TypeId
        Some(self.resolve_wasi_type(&return_type))
    }

    /// Resolve a WASI AST type to a `TypeId`
    fn resolve_wasi_type(&mut self, ty: &Type) -> TypeId {
        match ty {
            Type::Named(named) => match named.name.as_str() {
                "String" => self.get_string_struct_type(),
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
                "bool" => TypeTable::BOOL,
                // Type aliases from WASI (e.g., Instant, Duration)
                _ => {
                    // Clone to avoid borrow checker issues
                    let aliased = self.wasi_registry.get_type_alias(&named.name).cloned();
                    if let Some(aliased) = aliased {
                        self.resolve_wasi_type(&aliased)
                    } else {
                        // Resource types are represented as i32 handles
                        TypeTable::I32
                    }
                }
            },
            Type::Generic(generic) => match generic.name.as_str() {
                "Array" if generic.args.len() == 1 => {
                    let elem_type = self.resolve_wasi_type(&generic.args[0]);
                    self.type_table.borrow_mut().make_array(elem_type)
                }
                "Option" if generic.args.len() == 1 => {
                    let inner_type = self.resolve_wasi_type(&generic.args[0]);
                    self.type_table
                        .borrow_mut()
                        .intern(ResolvedType::Option(inner_type))
                }
                _ => TypeTable::UNIT,
            },
            Type::Tuple(types) => {
                let resolved: Vec<TypeId> =
                    types.iter().map(|t| self.resolve_wasi_type(t)).collect();
                self.type_table
                    .borrow_mut()
                    .intern(ResolvedType::Tuple(resolved))
            }
            _ => TypeTable::UNIT,
        }
    }

    /// Get the String struct type (from prelude)
    fn get_string_struct_type(&mut self) -> TypeId {
        self.type_table
            .borrow_mut()
            .make_struct("String".to_string(), ModuleSource::core("prelude"))
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
    /// primitive types and type aliases. For generic types, use `resolve_type` instead.
    /// Resolve a method call
    fn resolve_method_call(
        &mut self,
        method_call: &ast::MethodCallExpr,
        ctx: &mut FunctionContext,
    ) -> TirExpr {
        // Check for IndexMut desugaring: container[i].method() where method needs &mut self
        // We need to detect this BEFORE resolving the receiver, because resolve_index
        // would otherwise generate Index::index instead of IndexMut::index_mut
        if let ast::Expr::Index(index_expr) = &method_call.receiver
            && let Some(result) =
                self.try_resolve_index_mut_method_call(index_expr, method_call, ctx)
        {
            return result;
        }

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
            ResolvedType::Struct {
                name,
                module_source,
            } => (name.clone(), module_source.to_path()),
            ResolvedType::GenericInstance {
                name,
                module_source,
                ..
            } => (name.clone(), module_source.to_path()),
            _ => (self.mangle_type_name(base_type_id), vec![]),
        };

        // Look up method info based on receiver type
        // First try inherent method, then trait methods
        let mut method_info = self.lookup_method_info(receiver.type_id, &method_call.method);
        let mut trait_name: Option<String> = None;

        // Extract receiver type args for generic types (used for resolving associated types)
        let receiver_type_args_for_trait: Option<Vec<TypeId>> =
            match self.type_table.borrow().get(base_type_id).clone() {
                ResolvedType::GenericInstance { type_args, .. } if !type_args.is_empty() => {
                    Some(type_args)
                }
                _ => None,
            };

        // If inherent method not found, try trait methods
        if method_info.is_none()
            && let Some((found_trait, info)) = self.find_trait_method_for_type(
                &struct_name,
                &method_call.method,
                &module_path,
                receiver_type_args_for_trait.as_deref(),
            )
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
        // If no explicit type args, try to infer from arguments
        let method_type_args = if type_args.is_empty() {
            // Try to infer method type args from actual arguments
            self.infer_method_type_args(receiver.type_id, &method_call.method, &args, impl_offset)
        } else {
            type_args.clone()
        };

        if !method_type_args.is_empty() {
            subst_ctx = subst_ctx.with_method_args(&method_type_args, impl_offset);
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
                    let mangled = format!("Array<{elem_name}>");
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

        // Convert method type args to string names for method_info
        // Use inferred type args if available, otherwise use explicit type args
        let method_type_arg_names: Vec<String> = method_type_args
            .iter()
            .map(|t| self.mangle_type_name(*t))
            .collect();

        TirExpr::new(
            TirExprKind::MethodCall {
                receiver: Box::new(receiver),
                func: FunctionRef::External {
                    module_source: self.current_module_source.clone(),
                    name: mangled_method_name,
                    monomorph_info,
                    method_info: Some(LocalMethodName::with_method_type_args(
                        receiver_struct_name,
                        trait_name,
                        method_call.method.clone(),
                        method_type_arg_names,
                    )),
                },
                type_args: method_type_args, // Use inferred type args
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
            module_source: _,
        } = self.type_table.borrow().get(target_type_id).clone()
        {
            // Look up the variant case info
            if let Some(variant_info) = self.variant_cases.get(&name) {
                // Find the case by name
                if let Some((case_index, case_data)) = variant_info
                    .cases
                    .iter()
                    .enumerate()
                    .find(|(_, c)| c.name == static_call.method)
                {
                    // Validate argument count
                    if args.len() != case_data.field_types.len() {
                        self.errors.push(TypeError::ArgumentCountMismatch {
                            expected: case_data.field_types.len(),
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
                            case_name: case_data.name.clone(),
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
                ResolvedType::Struct {
                    name,
                    module_source,
                } => (name.clone(), module_source.to_path(), name.clone(), vec![]),
                ResolvedType::GenericInstance {
                    name,
                    module_source,
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
                        module_source.to_path(),
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
        let monomorph_info = if struct_type_args.is_empty() {
            None
        } else {
            // Generic static method: track the original generic name
            let generic_name = format!("{}::{}", struct_name, static_call.method);
            Some(MonomorphInfo {
                generic_name,
                type_args: struct_type_args,
            })
        };

        TirExpr::new(
            TirExprKind::StaticCall {
                func: FunctionRef::External {
                    module_source: ModuleSource::from_path(&module_path),
                    name: mangled_func_name,
                    monomorph_info,
                    method_info: Some(LocalMethodName::new(
                        mangled_struct_name,
                        None, // Static methods are inherent, no trait
                        static_call.method.clone(),
                    )),
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
        let simple_name = format!("{struct_name}::{method_name}");
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
            for module in self.loaded_modules.values() {
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
        let mangled_name = format!("{struct_name}::{method_name}");

        // Check if it's registered in function_return_types (static methods are registered there)
        if self.function_return_types.contains_key(&mangled_name) {
            return true;
        }

        // Also check in loaded modules' impl blocks
        for module in self.loaded_modules.values() {
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

        // Determine module source for the struct
        let module_source = self.find_struct_module_source(struct_name);

        TirExpr::new(
            TirExprKind::StaticCall {
                func: FunctionRef::External {
                    module_source,
                    name: mangled_func_name.to_string(),
                    monomorph_info: None,
                    method_info: Some(LocalMethodName::new(
                        struct_name.to_string(),
                        None, // Static methods are inherent, no trait
                        method_name.to_string(),
                    )),
                },
                args: args.to_vec(),
            },
            return_type,
            span,
        )
    }

    /// Find the module source for a struct by name
    fn find_struct_module_source(&self, struct_name: &str) -> ModuleSource {
        // Check current module
        for item in &self.current_module_items {
            if let Item::Struct(s) = item
                && s.name == struct_name
            {
                return self.current_module_source.clone();
            }
        }

        // Check loaded modules
        for (path, module) in self.loaded_modules {
            for item in &module.items {
                if let Item::Struct(s) = item
                    && s.name == struct_name
                {
                    return ModuleSource::from_path(path);
                }
            }
        }

        // Default to current module source
        self.current_module_source.clone()
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
    /// Returns `MethodInfo` including return type and `self_kind`, or None if not found.
    fn lookup_method_info(
        &mut self,
        receiver_type: TypeId,
        method_name: &str,
    ) -> Option<MethodInfo> {
        // First, get the base (non-reference) type for method lookup
        let base_type_id = self.get_base_type(receiver_type);
        let base_type = self.type_table.borrow().get(base_type_id).clone();

        // Get the struct name, module path, and type args from the base type
        let (struct_name, module_path, receiver_type_args) = match &base_type {
            ResolvedType::Struct {
                name,
                module_source,
            } => (name.clone(), module_source.to_path(), None),
            // Generic instances like Box<i32> use the base name "Box" for method lookup
            ResolvedType::GenericInstance {
                name,
                module_source,
                type_args,
            } => (
                name.clone(),
                module_source.to_path(),
                if type_args.is_empty() {
                    None
                } else {
                    Some(type_args.clone())
                },
            ),
            // Primitive types have built-in methods like to_string()
            ResolvedType::Primitive(_) => {
                if method_name == "to_string" {
                    // Return String struct type - primitives use value receiver
                    let return_type = self
                        .type_table
                        .borrow()
                        .find_struct_type("String", &string_module_source())
                        .unwrap_or(TypeTable::UNKNOWN);
                    return Some(MethodInfo {
                        return_type,
                        self_kind: ast::SelfKind::None,
                    });
                }
                return None;
            }
            _ => return None,
        };

        // Build the mangled method name and look it up locally first
        let mangled_name = format!("{struct_name}::{method_name}");
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
                                // Set up type params for generic impls (e.g., impl Array<T>)
                                let old_type_params = std::mem::take(&mut self.current_type_params);
                                let mut impl_offset = 0u32;
                                if let Some(ref type_args) = receiver_type_args
                                    && let Type::Generic(generic) = &impl_block.ty
                                {
                                    impl_offset = type_args.len() as u32;
                                    for (i, arg) in generic.args.iter().enumerate() {
                                        if let Type::Named(named) = arg
                                            && i < type_args.len()
                                        {
                                            self.current_type_params.insert(
                                                named.name.clone(),
                                                (i as u32, type_args[i]),
                                            );
                                        }
                                    }
                                }

                                // Set up method-level type params (e.g., Acc in fold<Acc>)
                                // These get TypeParam types that will be substituted at call sites
                                for (i, type_param) in method.type_params.iter().enumerate() {
                                    let index = impl_offset + i as u32;
                                    let type_param_id = self.type_table.borrow_mut().intern(
                                        ResolvedType::TypeParam {
                                            name: type_param.name.clone(),
                                            index,
                                        },
                                    );
                                    self.current_type_params
                                        .insert(type_param.name.clone(), (index, type_param_id));
                                }

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

                                self.current_type_params = old_type_params;

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
            for module in self.loaded_modules.values() {
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
                                    // Set up type params for generic impls (e.g., impl Array<T>)
                                    let old_type_params =
                                        std::mem::take(&mut self.current_type_params);
                                    let mut impl_offset = 0u32;
                                    if let Some(ref type_args) = receiver_type_args
                                        && let Type::Generic(generic) = &impl_block.ty
                                    {
                                        impl_offset = type_args.len() as u32;
                                        for (i, arg) in generic.args.iter().enumerate() {
                                            if let Type::Named(named) = arg
                                                && i < type_args.len()
                                            {
                                                self.current_type_params.insert(
                                                    named.name.clone(),
                                                    (i as u32, type_args[i]),
                                                );
                                            }
                                        }
                                    }

                                    // Set up method-level type params (e.g., Acc in fold<Acc>)
                                    // These get TypeParam types that will be substituted at call sites
                                    for (i, type_param) in method.type_params.iter().enumerate() {
                                        let index = impl_offset + i as u32;
                                        let type_param_id = self.type_table.borrow_mut().intern(
                                            ResolvedType::TypeParam {
                                                name: type_param.name.clone(),
                                                index,
                                            },
                                        );
                                        self.current_type_params.insert(
                                            type_param.name.clone(),
                                            (index, type_param_id),
                                        );
                                    }

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

                                    self.current_type_params = old_type_params;

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

    /// Find the `self_kind` for a method in current module items
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

    /// Infer method type arguments from actual argument types.
    /// Returns a list of inferred type args matching the method's type params order.
    /// Uses the position of type params in parameter types to map actual arg types.
    fn infer_method_type_args(
        &self,
        receiver_type: TypeId,
        method_name: &str,
        args: &[TirExpr],
        impl_offset: u32,
    ) -> Vec<TypeId> {
        let base_type_id = self.get_base_type(receiver_type);
        let base_type = self.type_table.borrow().get(base_type_id).clone();

        let (struct_name, module_path) = match &base_type {
            ResolvedType::Struct {
                name,
                module_source,
            } => (name.clone(), module_source.to_path()),
            ResolvedType::GenericInstance {
                name,
                module_source,
                ..
            } => (name.clone(), module_source.to_path()),
            _ => return vec![],
        };

        // Search for the method in loaded modules
        let mut method_type_params: Vec<String> = Vec::new();
        let mut param_type_strs: Vec<String> = Vec::new();

        // Helper function to extract param info from method
        let extract_method_info = |method: &crate::ast::Function| -> (Vec<String>, Vec<String>) {
            let type_params: Vec<String> =
                method.type_params.iter().map(|p| p.name.clone()).collect();
            let params: Vec<String> = method
                .params
                .iter()
                // Skip self parameter (has SelfKind::Ref/MutRef and name "self")
                .filter(|p| {
                    !(matches!(
                        p.self_kind,
                        ast::SelfKind::Ref | ast::SelfKind::MutRef | ast::SelfKind::None
                    ) && p.name == "self")
                })
                .map(|p| self.get_type_name(&p.ty))
                .collect();
            (type_params, params)
        };

        // Check specific module first
        if !module_path.is_empty()
            && let Some(module) = self.loaded_modules.get(&module_path) {
                for item in &module.items {
                    if let Item::Impl(impl_block) = item
                        && impl_block.trait_type.is_none()
                    {
                        let impl_type_name = self.get_type_name(&impl_block.ty);
                        // Match impl type name: either exact match or the base name matches
                        // For generic types like ArrayIter<T>, match if base name "ArrayIter" matches
                        let impl_base_name =
                            impl_type_name.split('<').next().unwrap_or(&impl_type_name);
                        if impl_type_name == struct_name || impl_base_name == struct_name {
                            for method in &impl_block.methods {
                                if method.name == method_name && !method.type_params.is_empty() {
                                    let (tp, pp) = extract_method_info(method);
                                    method_type_params = tp;
                                    param_type_strs = pp;
                                    break;
                                }
                            }
                        }
                    }
                }
            }

        // Search all loaded modules if not found
        if method_type_params.is_empty() {
            for module in self.loaded_modules.values() {
                for item in &module.items {
                    if let Item::Impl(impl_block) = item
                        && impl_block.trait_type.is_none()
                    {
                        let impl_type_name = self.get_type_name(&impl_block.ty);
                        let impl_base_name =
                            impl_type_name.split('<').next().unwrap_or(&impl_type_name);
                        if impl_type_name == struct_name || impl_base_name == struct_name {
                            for method in &impl_block.methods {
                                if method.name == method_name && !method.type_params.is_empty() {
                                    let (tp, pp) = extract_method_info(method);
                                    method_type_params = tp;
                                    param_type_strs = pp;
                                    break;
                                }
                            }
                        }
                    }
                }
                if !method_type_params.is_empty() {
                    break;
                }
            }
        }

        if method_type_params.is_empty() {
            return vec![];
        }

        // Infer type args by matching type param names against param types and actual arg types
        let mut inferred: Vec<TypeId> = vec![TypeTable::UNKNOWN; method_type_params.len()];

        for (i, type_param_name) in method_type_params.iter().enumerate() {
            // Find the first parameter whose type matches this type param
            for (param_idx, param_type_str) in param_type_strs.iter().enumerate() {
                if param_idx >= args.len() {
                    continue;
                }

                if param_type_str == type_param_name {
                    // This param has type T (or Acc, etc.) - use the actual arg type
                    inferred[i] = args[param_idx].type_id;
                    break;
                }

                // Check if the type param appears in a function type's return position
                // e.g., for "fn(T) -> U" we can infer U from the closure's return type
                if param_type_str.starts_with("fn(") {
                    // Parse function type to extract return type
                    // Format: "fn(param1, param2, ...) -> ReturnType"
                    if let Some(arrow_pos) = param_type_str.find(" -> ") {
                        let return_type_str = &param_type_str[arrow_pos + 4..];
                        if return_type_str == type_param_name {
                            // The return type is our type param - infer from closure's return type
                            let arg_type = self.type_table.borrow().get(args[param_idx].type_id).clone();
                            if let ResolvedType::Function { return_type, .. } = arg_type {
                                inferred[i] = return_type;
                                break;
                            }
                        }
                    }
                }
            }
        }

        // Return only if we found at least some type args
        if inferred.iter().all(|&t| t == TypeTable::UNKNOWN) {
            vec![]
        } else {
            // Use impl_offset to verify - type params start after impl params
            let _ = impl_offset;
            inferred
        }
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
                if let ResolvedType::MutRef(_) = &receiver_type {
                    // Already &mut T, use as-is
                    receiver
                } else {
                    // Value T, need to add &mut
                    let mut_ref_type = self.type_table.borrow_mut().make_mut_ref(receiver.type_id);
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
    /// Returns (`trait_name`, `MethodInfo`) if found, None otherwise.
    /// This is used when an inherent method is not found.
    ///
    /// `receiver_type_args` should contain the concrete type arguments for generic receivers
    /// (e.g., `[i32]` for `Box_<i32>`). This is used to substitute type parameters when
    /// resolving associated types like `type Item = T`.
    fn find_trait_method_for_type(
        &mut self,
        struct_name: &str,
        method_name: &str,
        module_path: &[String],
        receiver_type_args: Option<&[TypeId]>,
    ) -> Option<(String, MethodInfo)> {
        let mut found_traits: Vec<(String, MethodInfo)> = Vec::new();

        // Collect impl blocks to check (avoiding borrow issues)
        // Include associated type bindings for resolving Self::* types
        let mut impl_blocks_to_check: Vec<(
            Type,
            Type,
            Vec<Function>,
            Vec<crate::ast::AssociatedTypeBinding>,
        )> = Vec::new();

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
                        impl_block.associated_types.clone(),
                    ));
                }
            }
        }

        // Also check all loaded modules
        for module in self.loaded_modules.values() {
            for item in &module.items {
                if let Item::Impl(impl_block) = item
                    && let Some(trait_type) = &impl_block.trait_type
                {
                    impl_blocks_to_check.push((
                        impl_block.ty.clone(),
                        trait_type.clone(),
                        impl_block.methods.clone(),
                        impl_block.associated_types.clone(),
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
                    impl_block.associated_types.clone(),
                ));
            }
        }

        // Now process the collected impl blocks with mutable access
        for (impl_ty, trait_type, methods, associated_types) in impl_blocks_to_check {
            let impl_struct_name = self.get_type_name(&impl_ty);
            if impl_struct_name == struct_name {
                // Set up type parameters for resolving generic associated types
                // e.g., for `impl Container for Box_<T>` called on `Box_<i32>`,
                // we need to map T -> i32 so `type Item = T` resolves to i32
                let old_type_params = std::mem::take(&mut self.current_type_params);
                if let Some(type_args) = receiver_type_args
                    && let Type::Generic(generic) = &impl_ty
                {
                    for (i, arg) in generic.args.iter().enumerate() {
                        if let Type::Named(named) = arg
                            && i < type_args.len()
                        {
                            // Map type param name to concrete type from receiver
                            self.current_type_params
                                .insert(named.name.clone(), (i as u32, type_args[i]));
                        }
                    }
                }

                // Set up associated type bindings for resolving Self::* types
                let old_associated_type_bindings =
                    std::mem::take(&mut self.current_associated_type_bindings);
                for binding in &associated_types {
                    let type_id = self.resolve_type(&binding.ty);
                    self.current_associated_type_bindings
                        .insert(binding.name.clone(), type_id);
                }

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

                // Restore associated type bindings and type params
                self.current_associated_type_bindings = old_associated_type_bindings;
                self.current_type_params = old_type_params;
            }
        }

        // Remove duplicates
        found_traits.dedup_by(|a, b| a.0 == b.0);

        // Return the first one found (if there are multiple, it would be ambiguous,
        // but we'll handle that later with explicit disambiguation syntax)
        found_traits.into_iter().next()
    }

    /// Find Index trait implementation for a type
    fn find_index_trait_impl(
        &mut self,
        struct_name: &str,
        base_type_id: TypeId,
        _index_type: TypeId,
    ) -> Option<IndexTraitInfo> {
        // Look for impl Index<...> for StructName
        self.find_indexing_trait_impl(struct_name, base_type_id, "Index", "index", "Output")
            .map(|(output_type, self_kind, trait_name)| IndexTraitInfo {
                output_type,
                self_kind,
                trait_name,
            })
    }

    /// Find `IndexAssign` trait implementation for a type
    fn find_index_assign_trait_impl(
        &mut self,
        struct_name: &str,
        base_type_id: TypeId,
        _index_type: TypeId,
    ) -> Option<IndexAssignTraitInfo> {
        // Look for impl IndexAssign<...> for StructName
        self.find_indexing_trait_impl(
            struct_name,
            base_type_id,
            "IndexAssign",
            "index_assign",
            "Input",
        )
        .map(|(input_type, self_kind, trait_name)| IndexAssignTraitInfo {
            _input_type: input_type,
            self_kind,
            trait_name,
        })
    }

    /// Find `IndexMut` trait implementation for a type
    fn find_index_mut_trait_impl(
        &mut self,
        struct_name: &str,
        base_type_id: TypeId,
        _index_type: TypeId,
    ) -> Option<IndexMutTraitInfo> {
        // Look for impl IndexMut<...> for StructName
        self.find_indexing_trait_impl(struct_name, base_type_id, "IndexMut", "index_mut", "Output")
            .map(|(output_type, self_kind, trait_name)| IndexMutTraitInfo {
                output_type,
                self_kind,
                trait_name,
            })
    }

    /// Find `IndexValue` trait implementation for a type
    fn find_index_value_trait_impl(
        &mut self,
        struct_name: &str,
        base_type_id: TypeId,
        _index_type: TypeId,
    ) -> Option<IndexValueTraitInfo> {
        // Look for impl IndexValue<...> for StructName
        self.find_indexing_trait_impl(
            struct_name,
            base_type_id,
            "IndexValue",
            "index_value",
            "Output",
        )
        .map(|(output_type, self_kind, trait_name)| IndexValueTraitInfo {
            output_type,
            self_kind,
            trait_name,
        })
    }

    /// Find `Eq` trait implementation for a type
    fn find_eq_trait_impl(
        &mut self,
        struct_name: &str,
        base_type_id: TypeId,
    ) -> Option<ComparisonTraitInfo> {
        self.find_comparison_trait_impl(struct_name, base_type_id, "Eq", "eq")
    }

    /// Find `Ord` trait implementation for a type
    fn find_ord_trait_impl(
        &mut self,
        struct_name: &str,
        base_type_id: TypeId,
    ) -> Option<ComparisonTraitInfo> {
        self.find_comparison_trait_impl(struct_name, base_type_id, "Ord", "lt")
    }

    /// Helper to find comparison trait implementations (`Eq` or `Ord`)
    fn find_comparison_trait_impl(
        &mut self,
        struct_name: &str,
        base_type_id: TypeId,
        trait_name: &str,
        method_name: &str,
    ) -> Option<ComparisonTraitInfo> {
        // Get concrete type arguments from the base type (for generic instances)
        let _concrete_type_args: Vec<TypeId> =
            if let ResolvedType::GenericInstance { type_args, .. } =
                self.type_table.borrow().get(base_type_id).clone()
            {
                type_args
            } else {
                Vec::new()
            };

        // Collect impl blocks to check
        let mut impl_blocks_to_check: Vec<(Type, Type, Vec<Function>)> = Vec::new();

        // Check all loaded modules
        for module in self.loaded_modules.values() {
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

        // Process collected impl blocks
        for (impl_ty, trait_type, methods) in impl_blocks_to_check {
            let impl_struct_name = self.get_type_name(&impl_ty);
            if impl_struct_name != struct_name {
                continue;
            }

            // Check if this is the target trait
            let found_trait_name = self.get_type_name(&trait_type);
            if found_trait_name != trait_name {
                continue;
            }

            // Find the method
            for method in &methods {
                if method.name == method_name {
                    let self_kind = method
                        .params
                        .first()
                        .map(|p| p.self_kind)
                        .unwrap_or(ast::SelfKind::None);

                    return Some(ComparisonTraitInfo {
                        self_kind,
                        trait_name: trait_name.to_string(),
                    });
                }
            }
        }

        None
    }

    /// Helper to find indexing trait implementations (Index, `IndexMut`, or `IndexAssign`)
    fn find_indexing_trait_impl(
        &mut self,
        struct_name: &str,
        base_type_id: TypeId,
        trait_base_name: &str,
        method_name: &str,
        assoc_type_name: &str,
    ) -> Option<(TypeId, ast::SelfKind, String)> {
        // Get concrete type arguments from the base type (for generic instances like Triple<i32>)
        let concrete_type_args: Vec<TypeId> =
            if let ResolvedType::GenericInstance { type_args, .. } =
                self.type_table.borrow().get(base_type_id).clone()
            {
                type_args
            } else {
                Vec::new()
            };
        // Collect impl blocks to check
        let mut impl_blocks_to_check: Vec<(
            Type,
            Type,
            Vec<Function>,
            Vec<crate::ast::AssociatedTypeBinding>,
        )> = Vec::new();

        // Check all loaded modules
        for module in self.loaded_modules.values() {
            for item in &module.items {
                if let Item::Impl(impl_block) = item
                    && let Some(trait_type) = &impl_block.trait_type
                {
                    impl_blocks_to_check.push((
                        impl_block.ty.clone(),
                        trait_type.clone(),
                        impl_block.methods.clone(),
                        impl_block.associated_types.clone(),
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
                    impl_block.associated_types.clone(),
                ));
            }
        }

        // Process collected impl blocks
        for (impl_ty, trait_type, methods, associated_types) in impl_blocks_to_check {
            let impl_struct_name = self.get_type_name(&impl_ty);
            if impl_struct_name != struct_name {
                continue;
            }

            // Check if this is the target trait (Index or IndexAssign)
            // Use base trait name (e.g., "Index" not "Index<i32>") for method mangling
            let trait_name = self.get_type_name(&trait_type);
            if !trait_name.starts_with(trait_base_name) {
                continue;
            }

            // Build type parameter mapping from impl_ty to concrete types
            // e.g., for `impl IndexValue<i32> for Triple<T>` with concrete type `Triple<i32>`
            // we build the mapping: {"T" -> i32}
            let type_param_mapping = Self::build_type_param_mapping(&impl_ty, &concrete_type_args);

            // Find the method
            for method in &methods {
                if method.name == method_name {
                    // Set up associated type bindings
                    let old_bindings = std::mem::take(&mut self.current_associated_type_bindings);
                    for binding in &associated_types {
                        // Resolve the associated type, substituting type parameters
                        let type_id =
                            self.resolve_type_with_param_mapping(&binding.ty, &type_param_mapping);
                        self.current_associated_type_bindings
                            .insert(binding.name.clone(), type_id);
                    }

                    // Get the associated type (Output or Input)
                    let assoc_type = self
                        .current_associated_type_bindings
                        .get(assoc_type_name)
                        .copied()
                        .unwrap_or(TypeTable::UNKNOWN);

                    let self_kind = method
                        .params
                        .first()
                        .map(|p| p.self_kind)
                        .unwrap_or(ast::SelfKind::None);

                    // Restore associated type bindings
                    self.current_associated_type_bindings = old_bindings;

                    // Return base trait name (e.g., "IndexValue" not "IndexValue<i32>")
                    let base_trait_name = strip_type_params(&trait_name).to_string();
                    return Some((assoc_type, self_kind, base_trait_name));
                }
            }
        }

        None
    }

    /// Build a mapping from type parameter names to concrete type IDs.
    /// For `impl Trait for Container<T>` with concrete type `Container<i32>`,
    /// returns `{"T" -> i32's TypeId}`.
    fn build_type_param_mapping(
        impl_ty: &Type,
        concrete_type_args: &[TypeId],
    ) -> HashMap<String, TypeId> {
        let mut mapping = HashMap::new();

        // Extract type parameter names from impl_ty
        let type_param_names: Vec<String> = match impl_ty {
            Type::Generic(g) => g
                .args
                .iter()
                .filter_map(|arg| {
                    if let Type::Named(n) = arg {
                        // Single uppercase letter or PascalCase names are likely type parameters
                        Some(n.name.clone())
                    } else {
                        None
                    }
                })
                .collect(),
            _ => Vec::new(),
        };

        // Map each type parameter name to its concrete type
        for (i, param_name) in type_param_names.into_iter().enumerate() {
            if let Some(&type_id) = concrete_type_args.get(i) {
                mapping.insert(param_name, type_id);
            }
        }

        mapping
    }

    /// Resolve a type, substituting type parameters using the provided mapping.
    fn resolve_type_with_param_mapping(
        &mut self,
        ty: &Type,
        type_param_mapping: &HashMap<String, TypeId>,
    ) -> TypeId {
        match ty {
            Type::Named(n) => {
                // Check if this is a type parameter that should be substituted
                if let Some(&type_id) = type_param_mapping.get(&n.name) {
                    return type_id;
                }
                // Otherwise, resolve normally
                self.resolve_type(ty)
            }
            Type::Generic(g) => {
                // Resolve generic type with substituted arguments
                let resolved_args: Vec<TypeId> = g
                    .args
                    .iter()
                    .map(|arg| self.resolve_type_with_param_mapping(arg, type_param_mapping))
                    .collect();

                // Find the base type and create a generic instance
                let base_name = &g.name;
                self.type_table
                    .borrow_mut()
                    .intern(ResolvedType::GenericInstance {
                        name: base_name.clone(),
                        module_source: self.current_module_source.clone(),
                        type_args: resolved_args,
                    })
            }
            Type::Reference(inner) => {
                let inner_id = self.resolve_type_with_param_mapping(inner, type_param_mapping);
                self.type_table.borrow_mut().make_ref(inner_id)
            }
            Type::MutReference(inner) => {
                let inner_id = self.resolve_type_with_param_mapping(inner, type_param_mapping);
                self.type_table.borrow_mut().make_mut_ref(inner_id)
            }
            // For other types, fall back to normal resolution
            _ => self.resolve_type(ty),
        }
    }

    /// Try to resolve a method call on an index expression using `IndexMut`.
    /// Returns Some(TirExpr) if the method needs &mut self and the type implements `IndexMut`.
    /// Returns None if we should fall back to normal resolution (using Index).
    fn try_resolve_index_mut_method_call(
        &mut self,
        index_expr: &ast::IndexExpr,
        method_call: &ast::MethodCallExpr,
        ctx: &mut FunctionContext,
    ) -> Option<TirExpr> {
        // First, resolve the indexed container to get its type
        let container_expr = self.resolve_expr(&index_expr.expr, ctx);

        // Check if this is an Array type (Arrays use optimized direct access, not traits)
        let is_array = self
            .type_table
            .borrow()
            .as_array(container_expr.type_id)
            .is_some();
        if is_array {
            return None; // Use normal resolution for arrays
        }

        // Get base type (unwrap reference if needed)
        let base_type_id = match self.type_table.borrow().get(container_expr.type_id) {
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => *inner,
            _ => container_expr.type_id,
        };

        // Get struct name from base type
        let (struct_name, _module_path) = match self.type_table.borrow().get(base_type_id).clone() {
            ResolvedType::Struct {
                name,
                module_source,
            } => (name, module_source.to_path()),
            ResolvedType::GenericInstance {
                name,
                module_source,
                ..
            } => (name, module_source.to_path()),
            _ => return None, // Not a struct type
        };

        // Check if the type implements IndexMut
        let index_resolved = self.resolve_expr(&index_expr.index, ctx);
        let index_type = index_resolved.type_id;

        let index_mut_info =
            self.find_index_mut_trait_impl(&struct_name, base_type_id, index_type)?;

        // Now we need to check if the method being called requires &mut self
        // First, look up method info on the OUTPUT type (what IndexMut returns)
        let output_type = index_mut_info.output_type;
        let output_base_type_id = match self.type_table.borrow().get(output_type) {
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => *inner,
            _ => output_type,
        };

        let (output_struct_name, output_module_path, output_type_args) =
            match self.type_table.borrow().get(output_base_type_id).clone() {
                ResolvedType::Struct {
                    name,
                    module_source,
                } => (name, module_source.to_path(), None),
                ResolvedType::GenericInstance {
                    name,
                    module_source,
                    type_args,
                } => (
                    name,
                    module_source.to_path(),
                    if type_args.is_empty() {
                        None
                    } else {
                        Some(type_args)
                    },
                ),
                _ => (self.mangle_type_name(output_base_type_id), vec![], None),
            };

        // Look up method info to check if it needs &mut self
        let mut method_info = self.lookup_method_info(output_type, &method_call.method);
        let mut method_trait_name: Option<String> = None;

        if method_info.is_none()
            && let Some((found_trait, info)) = self.find_trait_method_for_type(
                &output_struct_name,
                &method_call.method,
                &output_module_path,
                output_type_args.as_deref(),
            )
        {
            method_trait_name = Some(found_trait);
            method_info = Some(info);
        }

        let MethodInfo {
            return_type,
            self_kind,
        } = method_info?;

        // Only use IndexMut if the method requires &mut self
        if self_kind != ast::SelfKind::MutRef {
            return None; // Method doesn't need &mut, fall back to Index
        }

        // Generate: container.index_mut(index).method(args)
        // Step 1: Create container.index_mut(index) call
        let receiver_for_index_mut = self.adjust_receiver_for_self_kind(
            container_expr,
            index_mut_info.self_kind,
            index_expr.span,
        );

        let mangled_index_mut_name =
            format!("{}^{}::index_mut", struct_name, index_mut_info.trait_name);

        // IndexMut returns &mut Output
        let mut_ref_output_type = self
            .type_table
            .borrow_mut()
            .make_mut_ref(index_mut_info.output_type);

        let index_mut_call = TirExpr::new(
            TirExprKind::MethodCall {
                receiver: Box::new(receiver_for_index_mut),
                func: FunctionRef::External {
                    module_source: self.current_module_source.clone(),
                    name: mangled_index_mut_name,
                    monomorph_info: None,
                    method_info: Some(LocalMethodName::new(
                        struct_name.clone(),
                        Some(index_mut_info.trait_name.clone()),
                        "index_mut".to_string(),
                    )),
                },
                type_args: vec![],
                args: vec![index_resolved],
            },
            mut_ref_output_type,
            index_expr.span,
        );

        // Step 2: Resolve method args
        let args: Vec<TirExpr> = method_call
            .args
            .iter()
            .map(|a| self.resolve_expr(a, ctx))
            .collect();

        // Step 3: Resolve method type args
        let type_args: Vec<TypeId> = method_call
            .type_args
            .iter()
            .map(|ty| self.resolve_type(ty))
            .collect();

        // Step 4: Create the method call on the result of index_mut
        // The receiver for the method is index_mut_call (which has type &mut Output)
        let receiver_for_method =
            self.adjust_receiver_for_self_kind(index_mut_call, self_kind, method_call.span);

        // Build mangled method name
        let mangled_method_name = match &method_trait_name {
            Some(trait_n) => format!("{}^{}::{}", output_struct_name, trait_n, method_call.method),
            None => format!("{}::{}", output_struct_name, method_call.method),
        };

        Some(TirExpr::new(
            TirExprKind::MethodCall {
                receiver: Box::new(receiver_for_method),
                func: FunctionRef::External {
                    module_source: self.current_module_source.clone(),
                    name: mangled_method_name,
                    monomorph_info: None,
                    method_info: Some(LocalMethodName::new(
                        output_struct_name,
                        method_trait_name,
                        method_call.method.clone(),
                    )),
                },
                type_args,
                args,
            },
            return_type,
            method_call.span,
        ))
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
                if let Some(struct_info) = self.struct_fields.get(&name) {
                    for (index, (fname, ftype)) in struct_info.fields.iter().enumerate() {
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
                if let Some(struct_info) = fields_clone {
                    for (index, (fname, ftype)) in struct_info.fields.iter().enumerate() {
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
                module_source,
                type_args: inner_args,
            } => {
                // Recursively substitute in nested generic instances
                let new_args: Vec<TypeId> = inner_args
                    .iter()
                    .map(|&arg| self.substitute_type_params(arg, type_args))
                    .collect();
                self.type_table
                    .borrow_mut()
                    .make_generic_instance(name, module_source, new_args)
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

        // For Array and custom types, look for Index or IndexValue trait implementation
        // (Array implements IndexValue<i32> with type Output = T)
        let struct_name = match &base_type {
            ResolvedType::Struct { name, .. } => name.clone(),
            ResolvedType::GenericInstance { name, .. } => name.clone(),
            _ => String::new(),
        };

        if !struct_name.is_empty() {
            let index_expr = self.resolve_expr(&index.index, ctx);
            let index_type = index_expr.type_id;

            // First, try Index trait (returns reference)
            if let Some(trait_info) =
                self.find_index_trait_impl(&struct_name, base_type_id, index_type)
            {
                // Generate: *expr.index(index_expr)
                // First, create the method call to .index(index_expr)
                let receiver = self.adjust_receiver_for_self_kind(
                    expr.clone(),
                    trait_info.self_kind,
                    index.span,
                );

                // Get the mangled method name: StructName^Index<IndexType>::index
                let mangled_method_name =
                    format!("{}^{}::index", struct_name, trait_info.trait_name);

                // The method returns &Output, so the type is Ref(output_type)
                let ref_output_type = self
                    .type_table
                    .borrow_mut()
                    .make_ref(trait_info.output_type);

                let method_call = TirExpr::new(
                    TirExprKind::MethodCall {
                        receiver: Box::new(receiver),
                        func: FunctionRef::External {
                            module_source: self.current_module_source.clone(),
                            name: mangled_method_name,
                            monomorph_info: None,
                            method_info: Some(LocalMethodName::new(
                                struct_name.clone(),
                                Some(trait_info.trait_name.clone()),
                                "index".to_string(),
                            )),
                        },
                        type_args: vec![],
                        args: vec![index_expr],
                    },
                    ref_output_type,
                    index.span,
                );

                // Dereference the result: *expr.index(...)
                return TirExpr::new(
                    TirExprKind::Unary {
                        op: TirUnaryOp::Deref,
                        expr: Box::new(method_call),
                    },
                    trait_info.output_type,
                    index.span,
                );
            }

            // Fallback: try IndexValue trait (returns value by copy)
            if let Some(trait_info) =
                self.find_index_value_trait_impl(&struct_name, base_type_id, index_type)
            {
                // Generate: expr.index_value(index_expr)
                let receiver = self.adjust_receiver_for_self_kind(
                    expr.clone(),
                    trait_info.self_kind,
                    index.span,
                );

                // Get the mangled method name: StructName^IndexValue<IndexType>::index_value
                let mangled_method_name =
                    format!("{}^{}::index_value", struct_name, trait_info.trait_name);

                // IndexValue returns Output directly (not a reference)
                return TirExpr::new(
                    TirExprKind::MethodCall {
                        receiver: Box::new(receiver),
                        func: FunctionRef::External {
                            module_source: self.current_module_source.clone(),
                            name: mangled_method_name,
                            monomorph_info: None,
                            method_info: Some(LocalMethodName::new(
                                struct_name.clone(),
                                Some(trait_info.trait_name.clone()),
                                "index_value".to_string(),
                            )),
                        },
                        type_args: vec![],
                        args: vec![index_expr],
                    },
                    trait_info.output_type,
                    index.span,
                );
            }
        }

        // Fallback: report error for unsupported indexing
        self.errors.push(TypeError::TypeMismatch {
            expected: "array or type implementing Index or IndexValue trait".to_string(),
            found: self.type_table.borrow().type_name(expr.type_id),
            span: index.span,
        });
        TirExpr::new(TirExprKind::Unit, TypeTable::UNKNOWN, index.span)
    }

    /// Extract the result type from a block (the type of its last expression, or Unit)
    fn block_result_type(block: &TirBlock) -> TypeId {
        block
            .stmts
            .last()
            .and_then(|s| match &s.kind {
                TirStmtKind::Expr(e) => Some(e.type_id),
                _ => None,
            })
            .unwrap_or(TypeTable::UNIT)
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

        // Extract types from both branches
        let then_type = Self::block_result_type(&then_block);
        let else_type = else_block
            .as_ref()
            .map_or(TypeTable::UNIT, Self::block_result_type);

        // Determine the result type
        let type_id = if then_type == else_type {
            // Types match exactly
            then_type
        } else if then_type == TypeTable::UNIT || else_type == TypeTable::UNIT {
            // If one branch is unit and else is missing, that's allowed for unit-typed if expressions
            if else_block.is_none() {
                // No else branch - require then branch to be unit
                if then_type != TypeTable::UNIT {
                    let type_name = self.type_table.borrow().type_name(then_type);
                    self.errors.push(TypeError::TypeMismatch {
                        expected: "()".to_string(),
                        found: type_name,
                        span: if_expr.then_block.span,
                    });
                }
                TypeTable::UNIT
            } else {
                // Both branches exist but types don't match - report error
                let then_name = self.type_table.borrow().type_name(then_type);
                let else_name = self.type_table.borrow().type_name(else_type);
                self.errors.push(TypeError::TypeMismatch {
                    expected: then_name,
                    found: else_name,
                    span: if_expr.else_block.as_ref().unwrap().span,
                });
                then_type
            }
        } else {
            // Types don't match
            let then_name = self.type_table.borrow().type_name(then_type);
            let else_name = self.type_table.borrow().type_name(else_type);
            self.errors.push(TypeError::TypeMismatch {
                expected: then_name,
                found: else_name,
                span: if_expr.else_block.as_ref().unwrap().span,
            });
            then_type
        };

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
    /// `Hello, {name}!` → `string_concat("Hello`, ", `to_string(name)`, "!")
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
                        let mangled_method_name = format!("{receiver_type_name}::to_string");
                        TirExpr::new(
                            TirExprKind::MethodCall {
                                receiver: Box::new(resolved.clone()),
                                func: FunctionRef::External {
                                    module_source: self.current_module_source.clone(),
                                    name: mangled_method_name,
                                    monomorph_info: None,
                                    method_info: Some(LocalMethodName::new(
                                        receiver_type_name,
                                        None, // to_string is an inherent method
                                        "to_string".to_string(),
                                    )),
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
                        module_source: ModuleSource::core("internal"),
                        name: "string_concat".to_string(),
                        monomorph_info: None,
                        method_info: None, // Free function
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

        // Get expected field types for coercion (for generic structs)
        let struct_field_types: Vec<(String, TypeId)> = self
            .struct_fields
            .get(&struct_name)
            .map(|info| info.fields.clone())
            .unwrap_or_default();

        // Resolve field expressions, converting tuple literals to arrays when needed
        let fields: Vec<TirStructField> = struct_lit
            .fields
            .iter()
            .enumerate()
            .map(|(index, field)| {
                let mut value = self.resolve_expr(&field.value, ctx);

                // Check if this is a tuple literal that should become an array
                // This happens when the struct field expects Array<T> and we have [...]
                if let TirExprKind::TupleLiteral { elements } = &value.kind {
                    // Check if the expected field type is Array<T>
                    if let Some((_, expected_type_id)) =
                        struct_field_types.iter().find(|(n, _)| n == &field.name)
                    {
                        let expected = self.type_table.borrow().get(*expected_type_id).clone();
                        if let ResolvedType::GenericInstance {
                            name,
                            type_args: expected_type_args,
                            ..
                        } = expected
                            && name == "Array"
                        {
                            // Determine the element type for the array
                            let elem_type = if elements.is_empty() {
                                // Empty array: use the expected element type from Array<T>
                                expected_type_args
                                    .first()
                                    .copied()
                                    .unwrap_or(TypeTable::UNKNOWN)
                            } else {
                                // Non-empty: check if all elements have the same type
                                let first_type = elements[0].type_id;
                                let all_same = elements.iter().all(|e| e.type_id == first_type);
                                if all_same {
                                    first_type
                                } else {
                                    // Not homogeneous, skip conversion
                                    return TirStructField {
                                        name: field.name.clone(),
                                        value,
                                        field_index: index as u32,
                                    };
                                }
                            };

                            // Convert tuple literal to array literal
                            let elements_clone =
                                if let TirExprKind::TupleLiteral { elements } = &value.kind {
                                    elements.clone()
                                } else {
                                    vec![]
                                };
                            let array_type = self.type_table.borrow_mut().make_array(elem_type);
                            value = TirExpr::new(
                                TirExprKind::ArrayLiteral {
                                    elements: elements_clone,
                                },
                                array_type,
                                value.span,
                            );
                        }
                    }
                }

                TirStructField {
                    name: field.name.clone(),
                    value,
                    field_index: index as u32,
                }
            })
            .collect();

        // Check if this is a generic struct and infer type arguments
        let (struct_type, mangled_struct_name) = if self.generic_struct_names.contains(&struct_name)
        {
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
            let struct_type = self.type_table.borrow_mut().make_generic_instance(
                struct_name.clone(),
                ModuleSource::from_path(&module_path),
                type_args.clone(),
            );
            // Build mangled name with type arguments
            let mangled_name = if type_args.is_empty() {
                format!("{struct_name}<>")
            } else {
                let arg_names: Vec<String> = type_args
                    .iter()
                    .map(|&t| self.type_table.borrow().type_name(t))
                    .collect();
                format!("{}<{}>", struct_name, arg_names.join(","))
            };
            (struct_type, mangled_name)
        } else {
            let struct_type = self
                .type_table
                .borrow_mut()
                .make_struct(struct_name.clone(), ModuleSource::from_path(&module_path));
            (struct_type, struct_name)
        };

        TirExpr::new(
            TirExprKind::StructLiteral {
                struct_type,
                struct_name: mangled_struct_name,
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
        let Some(struct_info) = self.struct_fields.get(struct_name) else {
            return vec![];
        };

        // Build a map from type param TypeId to concrete TypeId
        let mut type_param_map: HashMap<TypeId, TypeId> = HashMap::new();

        for (struct_field, (_, expected_type_id)) in fields.iter().zip(struct_info.fields.iter()) {
            let actual_type_id = struct_field.value.type_id;

            // Try to unify expected_type with actual_type to extract type params
            self.unify_types_for_inference(*expected_type_id, actual_type_id, &mut type_param_map);
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

    /// Unify expected type with actual type to extract type parameter mappings.
    /// This handles nested generic types like Array<T> where T is a type param.
    fn unify_types_for_inference(
        &self,
        expected: TypeId,
        actual: TypeId,
        type_param_map: &mut HashMap<TypeId, TypeId>,
    ) {
        let expected_type = self.type_table.borrow().get(expected).clone();
        let actual_type = self.type_table.borrow().get(actual).clone();

        match (&expected_type, &actual_type) {
            // Direct type parameter mapping
            (ResolvedType::TypeParam { .. }, _) => {
                type_param_map.insert(expected, actual);
            }
            // Generic instance: unify type arguments recursively
            (
                ResolvedType::GenericInstance {
                    name: expected_name,
                    type_args: expected_args,
                    ..
                },
                ResolvedType::GenericInstance {
                    name: actual_name,
                    type_args: actual_args,
                    ..
                },
            ) if expected_name == actual_name && expected_args.len() == actual_args.len() => {
                for (&exp_arg, &act_arg) in expected_args.iter().zip(actual_args.iter()) {
                    self.unify_types_for_inference(exp_arg, act_arg, type_param_map);
                }
            }
            // Array<K> (GenericInstance) with Tuple (homogeneous) - infer K from tuple element type
            (
                ResolvedType::GenericInstance {
                    name,
                    type_args: expected_args,
                    ..
                },
                ResolvedType::Tuple(actual_elems),
            ) if name == "Array" && expected_args.len() == 1 && !actual_elems.is_empty() => {
                // Check if all tuple elements have the same type
                let first_elem_type = actual_elems[0];
                let all_same = actual_elems.iter().all(|&e| e == first_elem_type);
                if all_same {
                    // Unify Array's element type param with the tuple's element type
                    self.unify_types_for_inference(
                        expected_args[0],
                        first_elem_type,
                        type_param_map,
                    );
                }
            }
            // Array types (Wado's Array<T>)
            (
                ResolvedType::BuiltinArray(expected_elem),
                ResolvedType::BuiltinArray(actual_elem),
            ) => {
                self.unify_types_for_inference(*expected_elem, *actual_elem, type_param_map);
            }
            // Ref types
            (ResolvedType::Ref(expected_inner), ResolvedType::Ref(actual_inner))
            | (ResolvedType::MutRef(expected_inner), ResolvedType::MutRef(actual_inner)) => {
                self.unify_types_for_inference(*expected_inner, *actual_inner, type_param_map);
            }
            // Option types
            (ResolvedType::Option(expected_inner), ResolvedType::Option(actual_inner)) => {
                self.unify_types_for_inference(*expected_inner, *actual_inner, type_param_map);
            }
            // Tuple types
            (ResolvedType::Tuple(expected_elems), ResolvedType::Tuple(actual_elems))
                if expected_elems.len() == actual_elems.len() =>
            {
                for (&exp, &act) in expected_elems.iter().zip(actual_elems.iter()) {
                    self.unify_types_for_inference(exp, act, type_param_map);
                }
            }
            // Function types: unify param types and return type
            (
                ResolvedType::Function {
                    params: expected_params,
                    return_type: expected_ret,
                    ..
                },
                ResolvedType::Function {
                    params: actual_params,
                    return_type: actual_ret,
                    ..
                },
            ) if expected_params.len() == actual_params.len() => {
                for (&exp, &act) in expected_params.iter().zip(actual_params.iter()) {
                    self.unify_types_for_inference(exp, act, type_param_map);
                }
                self.unify_types_for_inference(*expected_ret, *actual_ret, type_param_map);
            }
            // Other cases: no type params to extract
            _ => {}
        }
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

    /// Resolve a type from AST Type to `TypeId`
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

    /// Resolve a namespaced generic type like `builtin::array<T>` or `Self::Output`
    fn resolve_namespaced_generic_type(
        &mut self,
        namespaced: &crate::ast::NamespacedGenericType,
    ) -> TypeId {
        // Handle Self::AssociatedType
        if namespaced.namespace.as_str() == "Self" {
            // Look up the associated type binding
            if let Some(&type_id) = self.current_associated_type_bindings.get(&namespaced.name) {
                return type_id;
            }
            // If not found, it's an unknown associated type
            self.errors.push(TypeError::UnknownType {
                name: format!("Self::{}", namespaced.name),
                span: namespaced.span,
            });
            return TypeTable::ERROR;
        }

        if namespaced.namespace.as_str() == "builtin" {
            if namespaced.name.as_str() == "array" {
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
            } else {
                self.errors.push(TypeError::UnknownType {
                    name: format!("builtin::{}", namespaced.name),
                    span: namespaced.span,
                });
                TypeTable::ERROR
            }
        } else {
            self.errors.push(TypeError::UnknownType {
                name: format!("{}::{}", namespaced.namespace, namespaced.name),
                span: namespaced.span,
            });
            TypeTable::ERROR
        }
    }

    /// Resolve a named type
    fn resolve_named_type(&mut self, name: &str, _span: Span) -> TypeId {
        // Handle `Self` type reference in impl blocks
        if name == "Self" {
            if let Some(self_type) = self.current_self_type {
                return self_type;
            }
            // Self used outside of impl block - return Unknown
            return TypeTable::UNKNOWN;
        }

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
                } else if let Some(struct_info) = self.struct_fields.get(name) {
                    // It's a struct - use the module source where it was defined
                    self.type_table
                        .borrow_mut()
                        .make_struct(name.to_string(), struct_info.module_source.clone())
                } else if let Some(variant_info) = self.variant_cases.get(name) {
                    // It's a variant - use the module source where it was defined
                    self.type_table
                        .borrow_mut()
                        .make_variant(name.to_string(), variant_info.module_source.clone())
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
            _ => {
                // Check if it's a user-defined generic struct
                if self.generic_struct_names.contains(name) {
                    // Resolve type arguments
                    let type_args: Vec<TypeId> =
                        args.iter().map(|t| self.resolve_type(t)).collect();

                    // Get the module source where the struct was defined
                    let module_source = self
                        .struct_fields
                        .get(name)
                        .map(|info| info.module_source.clone())
                        .unwrap_or_else(|| self.current_module_source.clone());

                    // Create a GenericInstance type
                    self.type_table.borrow_mut().make_generic_instance(
                        name.to_string(),
                        module_source,
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

/// Convert AST `BinaryOp` to TIR `BinaryOp`
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

/// Convert AST `UnaryOp` to TIR `UnaryOp`
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
    module_source: ModuleSource,
    symbols: &SymbolTable,
    loaded_modules: &HashMap<Vec<String>, Module>,
) -> Result<TirModule, Vec<TypeError>> {
    let mut resolver = Resolver::new(symbols, loaded_modules);
    resolver.resolve_module(module, module_source)
}

/// Resolve all modules and return a Project ready for lowering.
///
/// This is the main entry point for the resolve phase. It resolves all modules
/// to TIR and packages them into a Project struct.
pub fn resolve_to_project(
    symbols: SymbolTable,
    modules: &HashMap<Vec<String>, Module>,
    entry_module_source: ModuleSource,
    implicit_modules: HashSet<Vec<String>>,
    module_name: String,
) -> Result<Project, Vec<TypeError>> {
    let tir_modules =
        Resolver::resolve_all_modules(&symbols, modules, entry_module_source.clone())?;

    // Convert Vec<String> to ModuleSource at the boundary
    // Use the provided entry_module_source for the entry module (empty path)
    // to preserve filename information
    let tir_modules_by_source: IndexMap<ModuleSource, TirModule> = tir_modules
        .into_iter()
        .map(|(path, tir)| {
            let module_source = if path.is_empty() {
                entry_module_source.clone()
            } else {
                ModuleSource::from_path(&path)
            };
            (module_source, tir)
        })
        .collect();
    let implicit_modules_by_source: HashSet<ModuleSource> = implicit_modules
        .into_iter()
        .map(|p| ModuleSource::from_path(&p))
        .collect();

    Ok(Project::new(
        entry_module_source,
        tir_modules_by_source,
        symbols,
        implicit_modules_by_source,
        module_name,
    ))
}
