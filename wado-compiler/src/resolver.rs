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

use crate::builtin_registry::BuiltinRegistry;
use crate::component_model::WasiRegistry;
use crate::name::{self as name, LocalMethodName, ModuleSource, mangle_generic_name};

use crate::ast::{
    self, BinaryOp, Block, BreakStmt, ContinueStmt, Expr, ExprStmt, Function, GlobalDecl, IfExpr,
    IfStmt, Item, LetStmt, Literal, LoopStmt, MatchArm, Module, Pattern, ReturnStmt, Stmt, Type,
    UnaryOp,
};
use crate::project::Project;
use crate::symbol::{SymbolKind, SymbolTable};
use crate::tir::{
    FunctionRef, MonomorphInfo, PrimitiveType, ResolvedType, SubstitutionContext, TirBinaryOp,
    TirBlock, TirCapture, TirExpr, TirExprKind, TirFunction, TirGlobal, TirLiteralPattern,
    TirMatchArm, TirModule, TirParam, TirPattern, TirStmt, TirStmtKind, TirStruct, TirStructField,
    TirTest, TirUnaryOp, TirVariantCase, TirVariantDecl, TypeId, TypeTable,
};
use crate::token::Span;

/// Helper to get the `ModuleSource` for String type (core:string)
fn string_module_source() -> ModuleSource {
    ModuleSource::core("string")
}

/// Struct field info: module source and field definitions
#[derive(Clone)]
struct StructFieldInfo {
    module_source: ModuleSource,
    /// Field definitions: (name, `type_id`) pairs
    fields: Vec<(String, TypeId)>,
    /// Type parameter bounds: (`param_name`, `trait_bounds`)
    /// E.g., for `struct Sorted<T: Ord>`, this would be `[("T", ["Ord"])]`
    type_param_bounds: Vec<(String, Vec<String>)>,
}

/// Variant case info: case name and payload type
#[derive(Clone)]
struct VariantCaseData {
    name: String,
    /// Payload type for this case. Unit variants have `()` (unit type) payload.
    payload: TypeId,
}

/// Variant info: module source, type parameters, and cases
#[derive(Clone)]
struct VariantInfo {
    module_source: ModuleSource,
    type_params: Vec<String>,
    cases: Vec<VariantCaseData>,
}

/// Enum case info: case name and discriminant index
#[derive(Clone)]
struct EnumCaseData {
    name: String,
    index: u32,
}

/// Enum info: module source and cases (enums have no type parameters or payloads)
#[derive(Clone)]
struct EnumInfo {
    module_source: ModuleSource,
    cases: Vec<EnumCaseData>,
}

/// Resource info: module source and method names
/// Note: This infrastructure was added for resource static methods but isn't fully used yet.
/// Keep it for when wasi:sockets registration is re-enabled.
#[allow(dead_code)]
#[derive(Clone)]
struct ResourceInfo {
    module_source: ModuleSource,
    /// Method names defined on this resource (both static and instance)
    methods: Vec<String>,
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
    UnknownIdentifier { name: String, span: Span },

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

    /// Trait bound not satisfied
    TraitBoundNotSatisfied {
        type_name: String,
        trait_name: String,
        param_name: String,
        span: Span,
    },

    /// Invalid pattern in context
    InvalidPattern { message: String, span: Span },
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
            TypeError::UnknownIdentifier { name, span } => {
                write!(
                    f,
                    "{}:{}: unknown identifier '{}'",
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
            TypeError::TraitBoundNotSatisfied {
                type_name,
                trait_name,
                param_name,
                span,
            } => {
                write!(
                    f,
                    "{}:{}: type '{}' does not implement trait '{}' required by bound on '{}'",
                    span.line, span.column, type_name, trait_name, param_name
                )
            }
            TypeError::InvalidPattern { message, span } => {
                write!(
                    f,
                    "{}:{}: invalid pattern: {}",
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

/// Method lookup result including return type, self parameter kind, and parameter types
#[derive(Debug, Clone)]
struct MethodInfo {
    return_type: TypeId,
    self_kind: ast::SelfKind,
    /// Parameter types (excluding self)
    param_types: Vec<TypeId>,
    /// If this method was inherited from a newtype's base type, the base type ID
    /// Used for method signature substitution
    inherited_from_base: Option<TypeId>,
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
    /// Enum case info (enum name -> (`module_path`, cases))
    enum_cases: HashMap<String, EnumInfo>,
    /// Resource info (resource name -> module source and methods)
    resource_types: HashMap<String, ResourceInfo>,
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
    /// Builtin registry for looking up builtin function return types
    builtin_registry: BuiltinRegistry,
    /// Global variables in the current module (name -> (type, `is_mutable`))
    current_module_globals: HashMap<String, (TypeId, bool)>,
    /// Imported globals (local name -> (source module, original name, type, `is_mutable`))
    imported_globals: HashMap<String, (ModuleSource, String, TypeId, bool)>,
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

/// Info about an arithmetic trait implementation (`Add`, `Sub`, `Mul`, `Div`, `Rem`)
struct ArithmeticTraitInfo {
    /// The Output associated type
    output_type: TypeId,
    /// Self kind for the method (&self)
    self_kind: ast::SelfKind,
    /// The trait name (e.g., "Add", "Sub")
    trait_name: String,
}

impl<'a> Resolver<'a> {
    /// Create a new resolver
    pub fn new(symbols: &'a SymbolTable, loaded_modules: &'a HashMap<Vec<String>, Module>) -> Self {
        let (wasi_registry, _) = WasiRegistry::build_from_stdlib();
        let type_table = Rc::new(RefCell::new(TypeTable::new()));
        let builtin_registry = BuiltinRegistry::build_from_stdlib(&type_table);
        Self {
            type_table,
            symbols,
            loaded_modules,
            type_aliases: HashMap::new(),
            struct_fields: HashMap::new(),
            variant_cases: HashMap::new(),
            enum_cases: HashMap::new(),
            resource_types: HashMap::new(),
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
            builtin_registry,
            current_module_globals: HashMap::new(),
            imported_globals: HashMap::new(),
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

        // Collect global variable names and types (before resolving functions that may reference them)
        self.current_module_globals.clear();
        self.imported_globals.clear();
        for item in &module.items {
            if let Item::Global(global_decl) = item {
                let ty = self.resolve_type(&global_decl.ty);
                self.current_module_globals
                    .insert(global_decl.name.clone(), (ty, global_decl.mutable));
            }
        }

        // Also collect imported globals from use declarations
        for item in &module.items {
            if let Item::Use(use_decl) = item {
                let source_module_path =
                    name::resolve_import_path(&module_source.to_path(), &use_decl.source);
                let source_module_source = ModuleSource::from_path(&source_module_path);

                // Look up the source module to find global declarations
                if let Some(source_module) = self.loaded_modules.get(&source_module_path) {
                    for use_item in &use_decl.items {
                        if let ast::UseItem::Simple { name, alias } = use_item {
                            // Check if this import refers to a global variable
                            if let Some(symbol) =
                                self.symbols.lookup_in_module(&source_module_path, name)
                                && let crate::symbol::SymbolKind::Global(global_sym) = &symbol.kind
                            {
                                // Find the global declaration in the source module to get its type
                                for src_item in &source_module.items {
                                    if let Item::Global(global_decl) = src_item
                                        && &global_decl.name == name
                                    {
                                        let ty = self.resolve_type(&global_decl.ty);
                                        let local_name = alias.as_ref().unwrap_or(name).clone();
                                        self.imported_globals.insert(
                                            local_name,
                                            (
                                                source_module_source.clone(),
                                                name.clone(),
                                                ty,
                                                global_sym.is_mut,
                                            ),
                                        );
                                        break;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

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
                Item::Test(test_decl) => {
                    let test_index = tir_module.tests.len();
                    if let Some((tir_func, tir_test)) =
                        self.resolve_test_decl(test_decl, test_index)
                    {
                        tir_module.add_function(tir_func);
                        tir_module.tests.push(tir_test);
                    }
                }
                Item::Global(global_decl) => {
                    if let Some(tir_global) = self.resolve_global(global_decl) {
                        tir_module.globals.push(tir_global);
                    }
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
        let mut enum_cases: HashMap<String, EnumInfo> = HashMap::new();
        let mut resource_types: HashMap<String, ResourceInfo> = HashMap::new();

        // First pass: collect struct, variant, enum, and resource names from all modules (for forward references)
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
                        // Extract type parameter bounds
                        let type_param_bounds: Vec<(String, Vec<String>)> = struct_decl
                            .type_params
                            .iter()
                            .map(|p| (p.name.clone(), p.bounds.clone()))
                            .collect();
                        struct_fields.insert(
                            struct_decl.name.clone(),
                            StructFieldInfo {
                                module_source: module_source.clone(),
                                fields: Vec::new(),
                                type_param_bounds,
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
                    Item::Enum(enum_decl) => {
                        // Insert with empty cases first - will be populated in second sub-pass
                        enum_cases.insert(
                            enum_decl.name.clone(),
                            EnumInfo {
                                module_source: module_source.clone(),
                                cases: Vec::new(),
                            },
                        );
                    }
                    Item::Resource(resource_decl) => {
                        resource_types.insert(
                            resource_decl.name.clone(),
                            ResourceInfo {
                                module_source: module_source.clone(),
                                methods: resource_decl
                                    .methods
                                    .iter()
                                    .map(|m| m.name.clone())
                                    .collect(),
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
                        // Extract type parameter names for generic structs
                        let type_params: Vec<String> = struct_decl
                            .type_params
                            .iter()
                            .map(|p| p.name.clone())
                            .collect();
                        for field in &struct_decl.fields {
                            // Use resolve_type_static_with_params for generic structs
                            // so that type params like K in Node<K> become TypeParam types
                            let type_id = if type_params.is_empty() {
                                Self::resolve_type_static(
                                    &field.ty,
                                    &mut type_table.borrow_mut(),
                                    &type_aliases,
                                    &struct_fields,
                                )
                            } else {
                                Self::resolve_type_static_with_params(
                                    &field.ty,
                                    &mut type_table.borrow_mut(),
                                    &type_aliases,
                                    &struct_fields,
                                    &type_params,
                                )
                            };
                            fields.push((field.name.clone(), type_id));
                        }
                        // Extract type parameter bounds
                        let type_param_bounds: Vec<(String, Vec<String>)> = struct_decl
                            .type_params
                            .iter()
                            .map(|p| (p.name.clone(), p.bounds.clone()))
                            .collect();
                        // Update the struct_fields entry with actual fields
                        struct_fields.insert(
                            struct_decl.name.clone(),
                            StructFieldInfo {
                                module_source: module_source.clone(),
                                fields,
                                type_param_bounds,
                            },
                        );
                    }
                    Item::Type(type_alias) => {
                        // Resolve the base type
                        let base_type_id = Self::resolve_type_static(
                            &type_alias.ty,
                            &mut type_table.borrow_mut(),
                            &type_aliases,
                            &struct_fields,
                        );
                        // Create a newtype wrapping the base type
                        let newtype_id = type_table.borrow_mut().make_newtype(
                            type_alias.name.clone(),
                            module_source.clone(),
                            base_type_id,
                        );
                        type_aliases.insert(type_alias.name.clone(), newtype_id);
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
                            // Each variant case has exactly one payload type.
                            // Unit variants have `()` (unit type) payload.
                            let payload = if let Some(payload_ty) = &case.payload {
                                // Use resolve_type_static_with_params for variant payloads
                                // so that type params like T in Ok(T) become TypeParam types
                                Self::resolve_type_static_with_params(
                                    payload_ty,
                                    &mut type_table.borrow_mut(),
                                    &type_aliases,
                                    &struct_fields,
                                    &type_params,
                                )
                            } else {
                                // Unit variant: payload is unit type
                                TypeTable::UNIT
                            };
                            cases.push(VariantCaseData {
                                name: case.name.clone(),
                                payload,
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
                    Item::Enum(enum_decl) => {
                        // Populate enum cases (no field types, just names and indices)
                        let cases: Vec<EnumCaseData> = enum_decl
                            .cases
                            .iter()
                            .enumerate()
                            .map(|(index, case)| EnumCaseData {
                                name: case.name.clone(),
                                index: index as u32,
                            })
                            .collect();
                        enum_cases.insert(
                            enum_decl.name.clone(),
                            EnumInfo {
                                module_source: module_source.clone(),
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
            let builtin_registry = BuiltinRegistry::build_from_stdlib(&type_table);
            let mut resolver = Resolver {
                type_table: Rc::clone(&type_table),
                symbols,
                loaded_modules: modules,
                type_aliases: type_aliases.clone(),
                struct_fields: struct_fields.clone(),
                variant_cases: variant_cases.clone(),
                enum_cases: enum_cases.clone(),
                resource_types: resource_types.clone(),
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
                builtin_registry,
                current_module_globals: HashMap::new(),
                imported_globals: HashMap::new(),
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
                    ..
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

    /// Static version of `resolve_type` with type parameters for variant payload resolution.
    /// This is similar to `resolve_type_static` but also handles type parameters (like T, E)
    /// that appear in generic variant definitions (like `Result<T, E>`).
    fn resolve_type_static_with_params(
        ty: &Type,
        type_table: &mut TypeTable,
        type_aliases: &HashMap<String, TypeId>,
        struct_fields: &HashMap<String, StructFieldInfo>,
        type_params: &[String],
    ) -> TypeId {
        match ty {
            Type::Named(named) => {
                // Check type aliases first
                if let Some(&alias_type_id) = type_aliases.get(&named.name) {
                    return alias_type_id;
                }

                // Check if it's a type parameter (e.g., T in Result<T, E>)
                if let Some(index) = type_params.iter().position(|p| p == &named.name) {
                    return type_table.make_type_param(named.name.clone(), index as u32);
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
                    let inner = Self::resolve_type_static_with_params(
                        &generic.args[0],
                        type_table,
                        type_aliases,
                        struct_fields,
                        type_params,
                    );
                    type_table.intern(ResolvedType::Option(inner))
                }
                _ => {
                    // Check if it's a generic struct type
                    if let Some(info) = struct_fields.get(&generic.name) {
                        // Resolve type arguments
                        let type_args: Vec<TypeId> = generic
                            .args
                            .iter()
                            .map(|arg| {
                                Self::resolve_type_static_with_params(
                                    arg,
                                    type_table,
                                    type_aliases,
                                    struct_fields,
                                    type_params,
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
                let inner_type = Self::resolve_type_static_with_params(
                    inner,
                    type_table,
                    type_aliases,
                    struct_fields,
                    type_params,
                );
                type_table.make_ref(inner_type)
            }
            Type::MutReference(inner) => {
                let inner_type = Self::resolve_type_static_with_params(
                    inner,
                    type_table,
                    type_aliases,
                    struct_fields,
                    type_params,
                );
                type_table.make_mut_ref(inner_type)
            }
            Type::NamespacedGeneric(namespaced) => {
                // Handle builtin::array<T>
                if namespaced.namespace == "builtin"
                    && namespaced.name == "array"
                    && let Some(elem_ty) = namespaced.args.first()
                {
                    let elem = Self::resolve_type_static_with_params(
                        elem_ty,
                        type_table,
                        type_aliases,
                        struct_fields,
                        type_params,
                    );
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
        for (path, loaded_module) in self.loaded_modules {
            let module_source = ModuleSource::from_path(path);
            for item in &loaded_module.items {
                if let Item::Type(type_alias) = item {
                    // Only add if not already present (main module takes priority)
                    if !self.type_aliases.contains_key(&type_alias.name) {
                        // Resolve the base type
                        let base_type_id = self.resolve_type(&type_alias.ty);
                        // Create a newtype wrapping the base type
                        let newtype_id = self.type_table.borrow_mut().make_newtype(
                            type_alias.name.clone(),
                            module_source.clone(),
                            base_type_id,
                        );
                        self.type_aliases
                            .insert(type_alias.name.clone(), newtype_id);
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
                    // Extract type parameter bounds
                    let type_param_bounds: Vec<(String, Vec<String>)> = struct_decl
                        .type_params
                        .iter()
                        .map(|p| (p.name.clone(), p.bounds.clone()))
                        .collect();
                    self.struct_fields.insert(
                        struct_decl.name.clone(),
                        StructFieldInfo {
                            module_source: self.current_module_source.clone(),
                            fields,
                            type_param_bounds,
                        },
                    );

                    // Restore type params scope
                    self.current_type_params = old_type_params;
                }
                Item::Type(type_alias) => {
                    // Resolve the base type
                    let base_type_id = self.resolve_type(&type_alias.ty);
                    // Create a newtype wrapping the base type
                    let newtype_id = self.type_table.borrow_mut().make_newtype(
                        type_alias.name.clone(),
                        self.current_module_source.clone(),
                        base_type_id,
                    );
                    self.type_aliases
                        .insert(type_alias.name.clone(), newtype_id);
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

                    // Collect variant cases with resolved payload types
                    let mut cases = Vec::new();
                    for case in &variant_decl.cases {
                        // Each variant case has exactly one payload type.
                        // Unit variants have `()` (unit type) payload.
                        let payload = if let Some(payload_ty) = &case.payload {
                            self.resolve_type(payload_ty)
                        } else {
                            TypeTable::UNIT
                        };
                        cases.push(VariantCaseData {
                            name: case.name.clone(),
                            payload,
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
                Item::Enum(enum_decl) => {
                    // Collect enum cases (no field types, just names and indices)
                    let cases: Vec<EnumCaseData> = enum_decl
                        .cases
                        .iter()
                        .enumerate()
                        .map(|(index, case)| EnumCaseData {
                            name: case.name.clone(),
                            index: index as u32,
                        })
                        .collect();
                    self.enum_cases.insert(
                        enum_decl.name.clone(),
                        EnumInfo {
                            module_source: self.current_module_source.clone(),
                            cases,
                        },
                    );
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

        // Convert AST type params to TIR type params (while type params still in scope)
        let type_params: Vec<crate::tir::TirTypeParam> = struct_decl
            .type_params
            .iter()
            .enumerate()
            .map(|(i, p)| crate::tir::TirTypeParam {
                name: p.name.clone(),
                bounds: p.bounds.clone(),
                default: p.default.as_ref().map(|ty| self.resolve_type(ty)),
                index: i as u32,
            })
            .collect();

        // Restore previous type params scope
        self.current_type_params = old_type_params;

        TirStruct {
            name: struct_decl.name.clone(),
            is_pub: struct_decl.is_pub,
            type_params,
            monomorph_info: None, // Not from monomorphization
            fields,
            span: struct_decl.span,
        }
    }

    /// Resolve a global variable declaration
    fn resolve_global(&mut self, global_decl: &GlobalDecl) -> Option<TirGlobal> {
        // Resolve the type
        let ty = self.resolve_type(&global_decl.ty);

        // Create a minimal function context for resolving the initializer expression
        // Global initialization has no locals, but we need the context for expression resolution
        // The function name is used for #function compile-time literal (empty for global init)
        let mut ctx = FunctionContext::new(ty, format!("global:{}", global_decl.name));

        // Resolve the initializer expression with expected type for type inference
        let initializer =
            self.resolve_expr_with_expected_type(&global_decl.initializer, &mut ctx, Some(ty));

        // Type check: initializer type must match declared type
        if initializer.type_id != ty
            && initializer.type_id != TypeTable::UNKNOWN
            && ty != TypeTable::UNKNOWN
        {
            self.errors.push(TypeError::TypeMismatch {
                expected: self.type_table.borrow().type_name(ty),
                found: self.type_table.borrow().type_name(initializer.type_id),
                span: global_decl.initializer.span(),
            });
        }

        Some(TirGlobal {
            name: global_decl.name.clone(),
            ty,
            initializer,
            mutable: global_decl.mutable,
            is_pub: global_decl.is_pub,
            module_source: self.current_module_source.clone(),
            span: global_decl.span,
            is_nullable: false, // Set by lower phase for lazy-init reference types
        })
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

        // Resolve each case - each variant case has exactly one payload type
        let mut cases = Vec::new();
        for (index, case) in variant_decl.cases.iter().enumerate() {
            // Unit variants have `()` (unit type) payload
            let payload = if let Some(payload_ty) = &case.payload {
                self.resolve_type(payload_ty)
            } else {
                TypeTable::UNIT
            };
            cases.push(TirVariantCase {
                name: case.name.clone(),
                index: index as u32,
                payload,
                span: case.span,
            });
        }

        // Convert AST type params to TIR type params (while type params still in scope)
        let type_params: Vec<crate::tir::TirTypeParam> = variant_decl
            .type_params
            .iter()
            .enumerate()
            .map(|(i, p)| crate::tir::TirTypeParam {
                name: p.name.clone(),
                bounds: p.bounds.clone(),
                default: p.default.as_ref().map(|ty| self.resolve_type(ty)),
                index: i as u32,
            })
            .collect();

        // Restore previous type params scope
        self.current_type_params = old_type_params;

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

        // Convert AST type params to TIR type params (while type params still in scope)
        let type_params: Vec<crate::tir::TirTypeParam> = func
            .type_params
            .iter()
            .enumerate()
            .map(|(i, p)| crate::tir::TirTypeParam {
                name: p.name.clone(),
                bounds: p.bounds.clone(),
                default: p.default.as_ref().map(|ty| self.resolve_type(ty)),
                index: i as u32,
            })
            .collect();

        // Restore previous type params scope
        self.current_type_params = old_type_params;

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
            // Scratch local fields - computed by lower phase
            scratch_locals: Vec::new(),
            copy_source_types: std::collections::HashSet::new(),
            indirect_call_counts: std::collections::HashMap::new(),
            match_scrutinee_types: Vec::new(),
            let_pattern_types: Vec::new(),
            cm_export_info: None,
        })
    }

    /// Resolve a test declaration to a `TirFunction` and `TirTest`
    fn resolve_test_decl(
        &mut self,
        test_decl: &ast::TestDecl,
        test_index: usize,
    ) -> Option<(TirFunction, TirTest)> {
        // Generate function name: __test_{index} or __test_{name_snake_case}
        let function_name = match &test_decl.name {
            Some(name) => {
                // Convert test name to snake_case for function name
                let snake_name: String = name
                    .chars()
                    .map(|c| if c.is_alphanumeric() { c } else { '_' })
                    .collect::<String>()
                    .to_lowercase();
                format!("__test_{test_index}_{snake_name}")
            }
            None => format!("__test_{test_index}"),
        };

        // Create function context - tests have no parameters and return unit
        let return_type = TypeTable::UNIT;
        let mut ctx = FunctionContext::new(return_type, function_name.clone());

        // Resolve the test body
        let body = self.resolve_block(&test_decl.body, &mut ctx);

        let tir_func = TirFunction {
            name: function_name.clone(),
            is_pub: false, // Tests are not public
            type_params: vec![],
            impl_type_params: vec![],
            monomorph_info: None,
            method_info: None,
            params: vec![], // Tests have no parameters
            return_type,
            effects: vec![], // Tests can have any effects (they're allowed to do I/O)
            body: Some(body),
            span: test_decl.span,
            local_count: ctx.next_local,
            local_types: ctx.local_types,
            address_taken_locals: ctx.address_taken_locals,
            needed_copy_types: std::collections::HashSet::new(),
            scratch_locals: Vec::new(),
            copy_source_types: std::collections::HashSet::new(),
            indirect_call_counts: std::collections::HashMap::new(),
            match_scrutinee_types: Vec::new(),
            let_pattern_types: Vec::new(),
            cm_export_info: None,
        };

        let tir_test = TirTest {
            name: test_decl.name.clone(),
            function_name,
            line: test_decl.span.line,
            span: test_decl.span,
        };

        Some((tir_func, tir_test))
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
                            default: None, // Impl type params don't have defaults
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
                ast::SelfKind::Value => {
                    // self by value: use impl type directly
                    self.resolve_type(impl_type)
                }
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

        // Convert AST type params to TIR type params (while type params still in scope)
        let type_params: Vec<crate::tir::TirTypeParam> = func
            .type_params
            .iter()
            .enumerate()
            .map(|(i, p)| crate::tir::TirTypeParam {
                name: p.name.clone(),
                bounds: p.bounds.clone(),
                default: p.default.as_ref().map(|ty| self.resolve_type(ty)),
                index: i as u32,
            })
            .collect();

        // Restore previous type params scope
        self.current_type_params = old_type_params;

        // Store type parameters for generic methods (for call site substitution)
        if !func.type_params.is_empty() {
            self.generic_method_params
                .insert(mangled_name, type_param_list);
        }

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
                base_struct_name: struct_name.to_string(),
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
            scratch_locals: Vec::new(),
            copy_source_types: std::collections::HashSet::new(),
            indirect_call_counts: std::collections::HashMap::new(),
            match_scrutinee_types: Vec::new(),
            let_pattern_types: Vec::new(),
            cm_export_info: None,
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
            // While, For, ForOf are desugared to Loop in the desugar phase
            Stmt::While(_) => unreachable!("While should be desugared before resolving"),
            Stmt::For(_) => unreachable!("For should be desugared before resolving"),
            Stmt::ForOf(_) => unreachable!("ForOf should be desugared before resolving"),
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
                    if let ResolvedType::Struct { name, .. } = target_resolved {
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
                // Use expected type for numeric literal coercion
                let value =
                    self.resolve_expr_with_expected_type(&let_stmt.value, ctx, Some(target_type));
                (value, target_type)
            }
        } else {
            let value = self.resolve_expr(&let_stmt.value, ctx);
            (value.clone(), value.type_id)
        };

        // Type check: if type annotation is present, verify value type matches
        if let Some(_annotated_type) = &let_stmt.ty
            && value.type_id != type_id
            && value.type_id != TypeTable::UNKNOWN
        {
            // Allow null (Option<unknown>) to be assigned to Option<T>
            let is_null_to_option = {
                let type_table = self.type_table.borrow();
                matches!(
                    (type_table.get(value.type_id), type_table.get(type_id)),
                    (ResolvedType::Option(inner), ResolvedType::Option(_))
                        if *inner == TypeTable::UNKNOWN
                )
            };
            if !is_null_to_option {
                self.errors.push(TypeError::TypeMismatch {
                    expected: self.type_table.borrow().type_name(type_id),
                    found: self.type_table.borrow().type_name(value.type_id),
                    span: let_stmt.value.span(),
                });
            }
        }

        // Handle different pattern types
        match &let_stmt.pattern {
            ast::Pattern::Ident(name) => {
                let local_index = ctx.add_local(name.clone(), type_id, let_stmt.is_mut);
                TirStmt::new(
                    TirStmtKind::Let {
                        name: name.clone(),
                        local_index,
                        is_mut: let_stmt.is_mut,
                        is_reactive: let_stmt.is_reactive,
                        type_id,
                        value,
                    },
                    let_stmt.span,
                )
            }
            ast::Pattern::Tuple(_) => {
                // Tuple destructuring: let [a, b] = tuple_expr;
                let tir_pattern = self.resolve_let_pattern(
                    &let_stmt.pattern,
                    type_id,
                    let_stmt.is_mut,
                    let_stmt.span,
                    ctx,
                );
                TirStmt::new(
                    TirStmtKind::LetPattern {
                        pattern: tir_pattern,
                        is_mut: let_stmt.is_mut,
                        value,
                    },
                    let_stmt.span,
                )
            }
            ast::Pattern::Wildcard => {
                // Wildcard pattern: let _ = expr; - evaluate but don't bind
                // We still need a local to store the value temporarily
                TirStmt::new(TirStmtKind::Expr(value), let_stmt.span)
            }
            ast::Pattern::Literal(_) | ast::Pattern::Variant { .. } => {
                // These patterns are not valid in let statements
                self.errors.push(TypeError::InvalidPattern {
                    message: "literal and variant patterns are not allowed in let statements"
                        .to_string(),
                    span: let_stmt.span,
                });
                // Return a dummy statement
                TirStmt::new(TirStmtKind::Expr(value), let_stmt.span)
            }
        }
    }

    /// Resolve a let pattern (for tuple destructuring)
    fn resolve_let_pattern(
        &mut self,
        pattern: &ast::Pattern,
        type_id: TypeId,
        is_mut: bool,
        span: Span,
        ctx: &mut FunctionContext,
    ) -> TirPattern {
        match pattern {
            ast::Pattern::Ident(name) => {
                let local_index = ctx.add_local(name.clone(), type_id, is_mut);
                TirPattern::Binding {
                    name: name.clone(),
                    local_index,
                    type_id,
                }
            }
            ast::Pattern::Tuple(patterns) => {
                // Get element types from the tuple type
                let elem_types = {
                    let type_table = self.type_table.borrow();
                    if let ResolvedType::Tuple(elem_types) = type_table.get(type_id) {
                        elem_types.clone()
                    } else {
                        // Error: expected tuple type
                        self.errors.push(TypeError::TypeMismatch {
                            expected: "tuple type".to_string(),
                            found: type_table.type_name(type_id),
                            span,
                        });
                        vec![TypeTable::UNKNOWN; patterns.len()]
                    }
                };

                // Check length
                if patterns.len() != elem_types.len() {
                    self.errors.push(TypeError::TypeMismatch {
                        expected: format!("tuple with {} elements", elem_types.len()),
                        found: format!("pattern with {} elements", patterns.len()),
                        span,
                    });
                }

                // Resolve each sub-pattern with its corresponding element type
                let tir_patterns: Vec<TirPattern> = patterns
                    .iter()
                    .zip(
                        elem_types
                            .iter()
                            .chain(std::iter::repeat(&TypeTable::UNKNOWN)),
                    )
                    .map(|(p, &elem_type)| {
                        self.resolve_let_pattern(p, elem_type, is_mut, span, ctx)
                    })
                    .collect();

                TirPattern::Tuple(tir_patterns)
            }
            ast::Pattern::Wildcard => TirPattern::Wildcard,
            ast::Pattern::Literal(_) | ast::Pattern::Variant { .. } => {
                // These patterns are not valid in let statements
                self.errors.push(TypeError::InvalidPattern {
                    message: "literal and variant patterns are not allowed in let statements"
                        .to_string(),
                    span,
                });
                TirPattern::Wildcard
            }
        }
    }

    /// Resolve an expression statement
    fn resolve_expr_stmt(&mut self, expr_stmt: &ExprStmt, ctx: &mut FunctionContext) -> TirStmt {
        let expr = self.resolve_expr(&expr_stmt.expr, ctx);
        TirStmt::new(TirStmtKind::Expr(expr), expr_stmt.span)
    }

    /// Resolve a return statement
    fn resolve_return(&mut self, ret_stmt: &ReturnStmt, ctx: &mut FunctionContext) -> TirStmt {
        let return_type = ctx.return_type;
        let value = ret_stmt.value.as_ref().map(|expr| {
            // Use expected type for coercion (numeric literals, tuple to array, etc.)
            self.resolve_expr_with_expected_type(expr, ctx, Some(return_type))
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
            ast::Condition::Expr(expr) => {
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
            ast::Condition::Pattern { pattern, expr, .. } => {
                // Pattern match condition: if let Some(x) = expr { ... }
                let scrutinee = self.resolve_expr(expr, ctx);
                let scrutinee_type = scrutinee.type_id;

                // Enter scope for pattern bindings (they're only visible in then_block)
                ctx.enter_scope();

                // Resolve the pattern with type information from scrutinee
                let tir_pattern =
                    self.resolve_if_pattern(pattern, scrutinee_type, ctx, if_stmt.span);

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
        span: Span,
    ) -> TirPattern {
        match pattern {
            Pattern::Wildcard => TirPattern::Wildcard,
            Pattern::Ident(name) => {
                // The binding gets the scrutinee type (or inner type for Option patterns)
                let index = ctx.add_local(name.clone(), scrutinee_type, false);
                TirPattern::Binding {
                    name: name.clone(),
                    local_index: index,
                    type_id: scrutinee_type,
                }
            }
            Pattern::Literal(lit) => {
                let tir_lit = match lit {
                    Literal::Number(n) => {
                        // Float literals cannot be used in match patterns
                        if Self::is_float_only_literal(&n.repr) {
                            self.errors.push(TypeError::InvalidPattern {
                                message: "float literals cannot be used in match patterns"
                                    .to_string(),
                                span,
                            });
                            return TirPattern::Wildcard;
                        }
                        // Check if scrutinee type is unsigned
                        let is_unsigned = matches!(
                            self.type_table.borrow().get(scrutinee_type),
                            ResolvedType::Primitive(
                                PrimitiveType::U8
                                    | PrimitiveType::U16
                                    | PrimitiveType::U32
                                    | PrimitiveType::U64
                                    | PrimitiveType::U128
                            )
                        );
                        if is_unsigned {
                            match Self::parse_u128_literal(&n.repr) {
                                Ok(value) => TirLiteralPattern::U128(value),
                                Err(_) => TirLiteralPattern::U128(0),
                            }
                        } else {
                            match Self::parse_i128_literal(&n.repr) {
                                Ok(value) => TirLiteralPattern::I128(value),
                                Err(_) => TirLiteralPattern::I128(0),
                            }
                        }
                    }
                    Literal::Bool(b) => TirLiteralPattern::Bool(*b),
                    Literal::Char(c) => TirLiteralPattern::Char(*c),
                    Literal::String(s) => TirLiteralPattern::String(s.value.clone()),
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
                    .map(|(p, &ty)| self.resolve_if_pattern(p, ty, ctx, span))
                    .collect();
                TirPattern::Tuple(resolved)
            }
            Pattern::Variant {
                variant_name,
                bindings,
                span,
            } => {
                let resolved_type = self.type_table.borrow().get(scrutinee_type).clone();

                // Each variant case has exactly one payload type.
                // Determine the payload type for the variant case.
                let payload_type: TypeId = match &resolved_type {
                    // Option<T>: Some has inner type T, None has unit type
                    ResolvedType::Option(inner) => {
                        if variant_name == "Some" {
                            *inner
                        } else {
                            TypeTable::UNIT
                        }
                    }
                    // Non-generic variant
                    ResolvedType::Variant { name, .. } => {
                        self.get_variant_case_payload_type(name, variant_name, &[], *span)
                    }
                    // Generic variant instantiation
                    ResolvedType::GenericInstance {
                        name, type_args, ..
                    } => {
                        // Check if this is a variant (not a struct)
                        if self.variant_cases.contains_key(name) {
                            self.get_variant_case_payload_type(name, variant_name, type_args, *span)
                        } else {
                            self.errors.push(TypeError::TypeMismatch {
                                expected: "variant type".to_string(),
                                found: name.clone(),
                                span: *span,
                            });
                            TypeTable::UNKNOWN
                        }
                    }
                    _ => {
                        self.errors.push(TypeError::TypeMismatch {
                            expected: "variant type (Option or custom variant)".to_string(),
                            found: format!("{resolved_type:?}"),
                            span: *span,
                        });
                        TypeTable::UNKNOWN
                    }
                };

                // Single payload = single binding pattern.
                // For backward compatibility, we still accept `Some(x)` as single binding.
                let resolved_bindings: Vec<TirPattern> = if bindings.len() == 1 {
                    vec![self.resolve_if_pattern(&bindings[0], payload_type, ctx, *span)]
                } else if bindings.is_empty() {
                    // Unit case like `None` - no bindings
                    vec![]
                } else {
                    // Multiple bindings are deprecated with single payload design.
                    // Error will be caught by test fixture updates.
                    bindings
                        .iter()
                        .map(|p| self.resolve_if_pattern(p, TypeTable::UNKNOWN, ctx, *span))
                        .collect()
                };

                TirPattern::Variant {
                    enum_type: scrutinee_type,
                    variant_name: variant_name.clone(),
                    bindings: resolved_bindings,
                    payload_type,
                }
            }
        }
    }

    /// Get payload type for a variant case, substituting type parameters if needed
    fn get_variant_case_payload_type(
        &mut self,
        variant_name: &str,
        case_name: &str,
        type_args: &[TypeId],
        span: Span,
    ) -> TypeId {
        // Clone payload first to avoid borrow conflict with substitute_type_params
        let payload_opt = self.variant_cases.get(variant_name).and_then(|info| {
            info.cases
                .iter()
                .find(|case| case.name == case_name)
                .map(|case| case.payload)
        });

        if let Some(payload) = payload_opt {
            // Substitute type parameters with concrete types
            return self.substitute_type_params(payload, type_args);
        }

        // Check if variant exists but case not found
        if self.variant_cases.contains_key(variant_name) {
            self.errors.push(TypeError::TypeMismatch {
                expected: format!("valid case of variant {variant_name}"),
                found: case_name.to_string(),
                span,
            });
        } else {
            self.errors.push(TypeError::TypeMismatch {
                expected: "known variant type".to_string(),
                found: variant_name.to_string(),
                span,
            });
        }
        TypeTable::UNKNOWN
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
            // Matches expressions are desugared to if-let in the desugar phase
            Expr::Matches(_) => {
                panic!("Matches expression should have been desugared to if-let before resolver")
            }
        }
    }

    /// Parse an integer literal string into a u64 value
    /// Supports decimal, hex, binary, octal, and scientific notation (e.g., "1e10")
    fn parse_int_literal(repr: &str) -> Result<u64, String> {
        // Remove underscores for parsing
        let clean: String = repr.chars().filter(|&c| c != '_').collect();

        // Handle negative numbers by parsing as i64 and reinterpreting bits
        if clean.starts_with('-') {
            // Check for scientific notation in negative numbers
            if clean.to_lowercase().contains('e') {
                let value: f64 = clean
                    .parse()
                    .map_err(|_| format!("invalid integer literal: {repr}"))?;
                if value.fract() != 0.0 {
                    return Err(format!("integer literal has fractional part: {repr}"));
                }
                if value < i64::MIN as f64 || value > i64::MAX as f64 {
                    return Err(format!("integer literal out of range: {repr}"));
                }
                return Ok((value as i64) as u64);
            }
            let value: i64 = clean
                .parse()
                .map_err(|_| format!("invalid integer literal: {repr}"))?;
            // Reinterpret i64 bits as u64 for storage
            return Ok(value as u64);
        }

        if clean.starts_with("0x") || clean.starts_with("0X") {
            u64::from_str_radix(&clean[2..], 16).map_err(|_| format!("invalid hex literal: {repr}"))
        } else if clean.starts_with("0b") || clean.starts_with("0B") {
            u64::from_str_radix(&clean[2..], 2)
                .map_err(|_| format!("invalid binary literal: {repr}"))
        } else if clean.starts_with("0o") || clean.starts_with("0O") {
            u64::from_str_radix(&clean[2..], 8)
                .map_err(|_| format!("invalid octal literal: {repr}"))
        } else if clean.to_lowercase().contains('e') {
            // Scientific notation: parse as f64 first, then convert to u64
            let value: f64 = clean
                .parse()
                .map_err(|_| format!("invalid integer literal: {repr}"))?;
            if value.fract() != 0.0 {
                return Err(format!("integer literal has fractional part: {repr}"));
            }
            if value < 0.0 || value > u64::MAX as f64 {
                return Err(format!("integer literal out of range: {repr}"));
            }
            Ok(value as u64)
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

        // Handle hex/binary/octal literals as float values (not bit patterns)
        if clean.starts_with("0x") || clean.starts_with("0X") {
            let value = u64::from_str_radix(&clean[2..], 16)
                .map_err(|_| format!("invalid hex literal: {repr}"))?;
            return Ok(value as f64);
        } else if clean.starts_with("0b") || clean.starts_with("0B") {
            let value = u64::from_str_radix(&clean[2..], 2)
                .map_err(|_| format!("invalid binary literal: {repr}"))?;
            return Ok(value as f64);
        } else if clean.starts_with("0o") || clean.starts_with("0O") {
            let value = u64::from_str_radix(&clean[2..], 8)
                .map_err(|_| format!("invalid octal literal: {repr}"))?;
            return Ok(value as f64);
        }

        clean
            .parse()
            .map_err(|_| format!("invalid float literal: {repr}"))
    }

    /// Check if a number literal can only be a float (has decimal point or negative exponent)
    fn is_float_only_literal(repr: &str) -> bool {
        // Has decimal point → float only
        if repr.contains('.') {
            return true;
        }

        // Check for negative exponent (e.g., "1e-5")
        let lower = repr.to_lowercase();
        if let Some(e_pos) = lower.find('e') {
            let after_e = &repr[e_pos + 1..];
            if after_e.starts_with('-') {
                return true;
            }
        }

        false
    }

    /// Parse an unsigned integer literal into a u128 value
    /// Supports decimal, hex, binary, and octal formats
    fn parse_u128_literal(repr: &str) -> Result<u128, String> {
        let clean: String = repr.chars().filter(|&c| c != '_').collect();

        if clean.starts_with("0x") || clean.starts_with("0X") {
            u128::from_str_radix(&clean[2..], 16)
                .map_err(|_| format!("invalid hex literal: {repr}"))
        } else if clean.starts_with("0b") || clean.starts_with("0B") {
            u128::from_str_radix(&clean[2..], 2)
                .map_err(|_| format!("invalid binary literal: {repr}"))
        } else if clean.starts_with("0o") || clean.starts_with("0O") {
            u128::from_str_radix(&clean[2..], 8)
                .map_err(|_| format!("invalid octal literal: {repr}"))
        } else {
            clean
                .parse()
                .map_err(|_| format!("invalid integer literal: {repr}"))
        }
    }

    /// Parse a signed integer literal into an i128 value
    /// Supports decimal, hex, binary, and octal formats (negative decimals supported)
    fn parse_i128_literal(repr: &str) -> Result<i128, String> {
        let clean: String = repr.chars().filter(|&c| c != '_').collect();

        if clean.starts_with("0x") || clean.starts_with("0X") {
            // Hex literals are always positive, parse as u128 then convert
            let unsigned = u128::from_str_radix(&clean[2..], 16)
                .map_err(|_| format!("invalid hex literal: {repr}"))?;
            Ok(unsigned as i128)
        } else if clean.starts_with("0b") || clean.starts_with("0B") {
            let unsigned = u128::from_str_radix(&clean[2..], 2)
                .map_err(|_| format!("invalid binary literal: {repr}"))?;
            Ok(unsigned as i128)
        } else if clean.starts_with("0o") || clean.starts_with("0O") {
            let unsigned = u128::from_str_radix(&clean[2..], 8)
                .map_err(|_| format!("invalid octal literal: {repr}"))?;
            Ok(unsigned as i128)
        } else {
            // Decimal - may be negative
            clean
                .parse()
                .map_err(|_| format!("invalid integer literal: {repr}"))
        }
    }

    /// Unpack u128 into (low, high) pair for codegen
    fn unpack_u128(value: u128) -> (u64, u64) {
        (value as u64, (value >> 64) as u64)
    }

    /// Unpack i128 into (low, high) pair for codegen
    fn unpack_i128(value: i128) -> (u64, i64) {
        (value as u64, (value >> 64) as i64)
    }

    /// Get the clean representation of a literal (without underscores)
    fn clean_literal_repr(repr: &str) -> String {
        repr.chars().filter(|&c| c != '_').collect()
    }

    /// Resolve a literal expression
    fn resolve_literal(&mut self, lit: &ast::LiteralExpr, ctx: &FunctionContext) -> TirExpr {
        let (kind, type_id) = match &lit.value {
            Literal::Number(num_lit) => {
                // Default type: i32 if integer-compatible, f64 if float-only
                if Self::is_float_only_literal(&num_lit.repr) {
                    // Must be float (has decimal point or negative exponent)
                    match Self::parse_float_literal(&num_lit.repr) {
                        Ok(value) => (
                            TirExprKind::FloatLiteral {
                                value,
                                repr: num_lit.repr.clone(),
                            },
                            TypeTable::F64,
                        ),
                        Err(message) => {
                            self.errors.push(TypeError::InvalidLiteral {
                                message,
                                span: lit.span,
                            });
                            (
                                TirExprKind::FloatLiteral {
                                    value: 0.0,
                                    repr: num_lit.repr.clone(),
                                },
                                TypeTable::F64,
                            )
                        }
                    }
                } else {
                    // Can be integer (default to i32)
                    match Self::parse_int_literal(&num_lit.repr) {
                        Ok(value) => (
                            TirExprKind::IntLiteral {
                                value,
                                repr: num_lit.repr.clone(),
                            },
                            TypeTable::I32,
                        ),
                        Err(message) => {
                            self.errors.push(TypeError::InvalidLiteral {
                                message,
                                span: lit.span,
                            });
                            (
                                TirExprKind::IntLiteral {
                                    value: 0,
                                    repr: num_lit.repr.clone(),
                                },
                                TypeTable::I32,
                            )
                        }
                    }
                }
            }
            Literal::Bool(b) => (TirExprKind::BoolLiteral(*b), TypeTable::BOOL),
            Literal::Char(c) => (TirExprKind::CharLiteral(*c), TypeTable::CHAR),
            Literal::String(s) => {
                let string_type = self.get_string_struct_type();
                (TirExprKind::StringLiteral(s.value.clone()), string_type)
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
                    // Unit variant - payload must be unit type
                    let payload_is_unit = matches!(
                        self.type_table.borrow().get(case_data.payload),
                        ResolvedType::Unit
                    );
                    if !payload_is_unit {
                        self.errors.push(TypeError::ArgumentCountMismatch {
                            expected: 1,
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
                            payload: None, // Unit variant has no explicit payload
                        },
                        variant_type,
                        ident.span,
                    );
                }
            }

            // Check for enum case: Color::Red (enums have no payload)
            if let Some(enum_info) = self.enum_cases.get(prefix)
                && let Some(case_data) = enum_info.cases.iter().find(|c| c.name == suffix)
            {
                // Create enum type
                let enum_type = self
                    .type_table
                    .borrow_mut()
                    .make_enum(prefix.to_string(), enum_info.module_source.clone());

                return TirExpr::new(
                    TirExprKind::EnumConstruct {
                        enum_type,
                        case_index: case_data.index,
                        case_name: case_data.name.clone(),
                    },
                    enum_type,
                    ident.span,
                );
            }
        }

        // Check for global variables in current module
        if let Some(&(ty, _mutable)) = self.current_module_globals.get(&ident.name) {
            return TirExpr::new(
                TirExprKind::GlobalVarGet {
                    module_source: self.current_module_source.clone(),
                    name: ident.name.clone(),
                },
                ty,
                ident.span,
            );
        }

        // Check for imported global variables
        if let Some((source_module, original_name, ty, _mutable)) =
            self.imported_globals.get(&ident.name)
        {
            return TirExpr::new(
                TirExprKind::GlobalVarGet {
                    module_source: source_module.clone(),
                    name: original_name.clone(),
                },
                *ty,
                ident.span,
            );
        }

        // Check if it's a known function (function reference)
        if self.function_return_types.contains_key(&ident.name)
            || self.imported_functions.contains(&ident.name)
        {
            // It's a function reference - return Global
            // The function type and proper handling will be done later
            let module_source = if self.function_return_types.contains_key(&ident.name) {
                self.current_module_source.clone()
            } else {
                // For imported functions, look up the module source from symbols
                self.symbols
                    .lookup(&ident.name)
                    .map(|s| ModuleSource::from_path(&s.module_path))
                    .unwrap_or_else(ModuleSource::entry_point)
            };
            return TirExpr::new(
                TirExprKind::Global {
                    module_source,
                    name: ident.name.clone(),
                },
                TypeTable::UNKNOWN,
                ident.span,
            );
        }

        // Check if it's a prelude function (panic, unreachable)
        if matches!(ident.name.as_str(), "panic" | "unreachable") {
            return TirExpr::new(
                TirExprKind::Global {
                    module_source: ModuleSource::core("prelude"),
                    name: ident.name.clone(),
                },
                TypeTable::UNKNOWN,
                ident.span,
            );
        }

        // Unknown variable - report error
        self.errors.push(TypeError::UnknownIdentifier {
            name: ident.name.clone(),
            span: ident.span,
        });
        TirExpr::new(TirExprKind::Unit, TypeTable::ERROR, ident.span)
    }

    /// Resolve a binary expression
    fn resolve_binary(&mut self, binary: &ast::BinaryExpr, ctx: &mut FunctionContext) -> TirExpr {
        // Bidirectional coercion: if one operand is a numeric literal and the other is not,
        // resolve the non-literal first and use its type to coerce the literal
        let left_is_numeric_literal = self.is_numeric_literal(&binary.left);
        let right_is_numeric_literal = self.is_numeric_literal(&binary.right);

        let (left, right) = if left_is_numeric_literal && !right_is_numeric_literal {
            // Resolve right first, then coerce left to right's type
            let right = self.resolve_expr(&binary.right, ctx);
            let expected_type = if self.type_table.borrow().is_numeric(right.type_id) {
                Some(right.type_id)
            } else {
                None
            };
            let left = self.resolve_expr_with_expected_type(&binary.left, ctx, expected_type);
            (left, right)
        } else if right_is_numeric_literal && !left_is_numeric_literal {
            // Resolve left first, then coerce right to left's type
            let left = self.resolve_expr(&binary.left, ctx);
            let expected_type = if self.type_table.borrow().is_numeric(left.type_id) {
                Some(left.type_id)
            } else {
                None
            };
            let right = self.resolve_expr_with_expected_type(&binary.right, ctx, expected_type);
            (left, right)
        } else {
            // Both literals or both non-literals - resolve normally
            let left = self.resolve_expr(&binary.left, ctx);
            let right = self.resolve_expr(&binary.right, ctx);
            (left, right)
        };

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

        // Check if this is an arithmetic or bitwise operation on a non-primitive type
        // Non-primitives use Add/Sub/Mul/Div/Rem/BitAnd/BitOr/BitXor traits
        let is_arithmetic_or_bitwise = matches!(
            binary.op,
            BinaryOp::Add
                | BinaryOp::Sub
                | BinaryOp::Mul
                | BinaryOp::Div
                | BinaryOp::Mod
                | BinaryOp::BitAnd
                | BinaryOp::BitOr
                | BinaryOp::BitXor
        );

        if is_arithmetic_or_bitwise {
            // Get struct name for trait lookup
            let struct_name = match &left_type {
                ResolvedType::Struct { name, .. } => Some(name.clone()),
                ResolvedType::GenericInstance { name, .. } => Some(name.clone()),
                _ => None,
            };

            if let Some(struct_name) = struct_name {
                // Determine which trait and method to use based on operator
                let (trait_name, method_name) = match binary.op {
                    BinaryOp::Add => ("Add", "add"),
                    BinaryOp::Sub => ("Sub", "sub"),
                    BinaryOp::Mul => ("Mul", "mul"),
                    BinaryOp::Div => ("Div", "div"),
                    BinaryOp::Mod => ("Rem", "rem"),
                    BinaryOp::BitAnd => ("BitAnd", "bitand"),
                    BinaryOp::BitOr => ("BitOr", "bitor"),
                    BinaryOp::BitXor => ("BitXor", "bitxor"),
                    _ => unreachable!(),
                };

                // Find the arithmetic trait implementation
                if let Some(trait_info) = self.find_arithmetic_trait_impl(
                    &struct_name,
                    left.type_id,
                    trait_name,
                    method_name,
                ) {
                    // Adjust receiver for self kind (&self)
                    let receiver = self.adjust_receiver_for_self_kind(
                        left.clone(),
                        trait_info.self_kind,
                        binary.span,
                    );

                    // Create reference type for the argument (rhs: &Self)
                    let arg_ref_type = self
                        .type_table
                        .borrow_mut()
                        .intern(ResolvedType::Ref(right.type_id));

                    let arg_ref = TirExpr::new(
                        TirExprKind::Unary {
                            op: TirUnaryOp::Ref,
                            expr: Box::new(right.clone()),
                        },
                        arg_ref_type,
                        binary.span,
                    );

                    // Get the mangled method name: StructName^Add::add
                    let mangled_method_name =
                        format!("{}^{}::{}", struct_name, trait_info.trait_name, method_name);

                    return TirExpr::new(
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
                        trait_info.output_type,
                        binary.span,
                    );
                }
            }
        }

        // Check if this is a shift operation on a non-primitive type
        // Non-primitives use Shl/Shr traits (with rhs: u32, not &Self)
        let is_shift = matches!(binary.op, BinaryOp::Shl | BinaryOp::Shr);

        if is_shift {
            // Get struct name for trait lookup
            let struct_name = match &left_type {
                ResolvedType::Struct { name, .. } => Some(name.clone()),
                ResolvedType::GenericInstance { name, .. } => Some(name.clone()),
                _ => None,
            };

            if let Some(struct_name) = struct_name {
                // Determine which trait and method to use based on operator
                let (trait_name, method_name) = match binary.op {
                    BinaryOp::Shl => ("Shl", "shl"),
                    BinaryOp::Shr => ("Shr", "shr"),
                    _ => unreachable!(),
                };

                // Find the shift trait implementation
                if let Some(trait_info) = self.find_arithmetic_trait_impl(
                    &struct_name,
                    left.type_id,
                    trait_name,
                    method_name,
                ) {
                    // Adjust receiver for self kind (&self)
                    let receiver = self.adjust_receiver_for_self_kind(
                        left.clone(),
                        trait_info.self_kind,
                        binary.span,
                    );

                    // For shift operations, rhs is u32 (not &Self), so pass directly
                    // Get the mangled method name: StructName^Shl::shl
                    let mangled_method_name =
                        format!("{}^{}::{}", struct_name, trait_info.trait_name, method_name);

                    return TirExpr::new(
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
                            args: vec![right.clone()], // Pass rhs directly (u32)
                        },
                        trait_info.output_type,
                        binary.span,
                    );
                }
            }
        }

        let op = convert_binary_op(binary.op);

        // Type check for newtypes: if either operand is a newtype, they must be the same type
        // (This prevents mixing Meters and Seconds even though both wrap i32)
        {
            let type_table = self.type_table.borrow();
            let left_is_newtype =
                matches!(type_table.get(left.type_id), ResolvedType::Newtype { .. });
            let right_is_newtype =
                matches!(type_table.get(right.type_id), ResolvedType::Newtype { .. });

            if (left_is_newtype || right_is_newtype) && left.type_id != right.type_id {
                let left_name = type_table.type_name(left.type_id);
                let right_name = type_table.type_name(right.type_id);
                self.errors.push(TypeError::TypeMismatch {
                    expected: left_name,
                    found: right_name,
                    span: binary.span,
                });
            }
        }

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

        // Check for negation on non-primitive types that implement Neg trait
        if unary.op == UnaryOp::Neg {
            let expr_type = self.type_table.borrow().get(expr.type_id).clone();
            let struct_name = match &expr_type {
                ResolvedType::Struct { name, .. } => Some(name.clone()),
                ResolvedType::GenericInstance { name, .. } => Some(name.clone()),
                _ => None,
            };

            if let Some(struct_name) = struct_name {
                // Find the Neg trait implementation
                if let Some(trait_info) =
                    self.find_arithmetic_trait_impl(&struct_name, expr.type_id, "Neg", "neg")
                {
                    // Adjust receiver for self kind (&self)
                    let receiver = self.adjust_receiver_for_self_kind(
                        expr.clone(),
                        trait_info.self_kind,
                        unary.span,
                    );

                    // Get the mangled method name: StructName^Neg::neg
                    let mangled_method_name =
                        format!("{}^{}::neg", struct_name, trait_info.trait_name);

                    return TirExpr::new(
                        TirExprKind::MethodCall {
                            receiver: Box::new(receiver),
                            func: FunctionRef::External {
                                module_source: ModuleSource::core("prelude"),
                                name: mangled_method_name,
                                monomorph_info: None,
                                method_info: Some(LocalMethodName::new(
                                    struct_name.clone(),
                                    Some(trait_info.trait_name.clone()),
                                    "neg".to_string(),
                                )),
                            },
                            type_args: vec![],
                            args: vec![],
                        },
                        trait_info.output_type,
                        unary.span,
                    );
                }
            }
        }

        // Check for bitwise NOT on non-primitive types that implement BitNot trait
        if unary.op == UnaryOp::BitNot {
            let expr_type = self.type_table.borrow().get(expr.type_id).clone();
            let struct_name = match &expr_type {
                ResolvedType::Struct { name, .. } => Some(name.clone()),
                ResolvedType::GenericInstance { name, .. } => Some(name.clone()),
                _ => None,
            };

            if let Some(struct_name) = struct_name {
                // Find the BitNot trait implementation
                if let Some(trait_info) =
                    self.find_arithmetic_trait_impl(&struct_name, expr.type_id, "BitNot", "bitnot")
                {
                    // Adjust receiver for self kind (&self)
                    let receiver = self.adjust_receiver_for_self_kind(
                        expr.clone(),
                        trait_info.self_kind,
                        unary.span,
                    );

                    // Get the mangled method name: StructName^BitNot::bitnot
                    let mangled_method_name =
                        format!("{}^{}::bitnot", struct_name, trait_info.trait_name);

                    return TirExpr::new(
                        TirExprKind::MethodCall {
                            receiver: Box::new(receiver),
                            func: FunctionRef::External {
                                module_source: ModuleSource::core("prelude"),
                                name: mangled_method_name,
                                monomorph_info: None,
                                method_info: Some(LocalMethodName::new(
                                    struct_name.clone(),
                                    Some(trait_info.trait_name.clone()),
                                    "bitnot".to_string(),
                                )),
                            },
                            type_args: vec![],
                            args: vec![],
                        },
                        trait_info.output_type,
                        unary.span,
                    );
                }
            }
        }

        // Constant folding: fold -literal into a negative literal
        if unary.op == UnaryOp::Neg {
            match &expr.kind {
                TirExprKind::IntLiteral { value, repr } => {
                    // Fold -N into a negative literal
                    // Use wrapping negation to handle edge cases like -i64::MIN
                    // Store as u64 (two's complement representation)
                    let neg_value = (*value as i64).wrapping_neg() as u64;
                    return TirExpr::new(
                        TirExprKind::IntLiteral {
                            value: neg_value,
                            repr: format!("-{repr}"),
                        },
                        expr.type_id,
                        unary.span,
                    );
                }
                TirExprKind::FloatLiteral { value, repr } => {
                    // Fold -N.M into a negative float literal
                    return TirExpr::new(
                        TirExprKind::FloatLiteral {
                            value: -value,
                            repr: format!("-{repr}"),
                        },
                        expr.type_id,
                        unary.span,
                    );
                }
                // Handle -(N as T) -> (-N) as T for integer casts
                TirExprKind::Cast {
                    expr: inner,
                    target_type,
                } => {
                    if let TirExprKind::IntLiteral { value, repr } = &inner.kind {
                        let neg_value = (*value as i64).wrapping_neg() as u64;
                        let neg_literal = TirExpr::new(
                            TirExprKind::IntLiteral {
                                value: neg_value,
                                repr: format!("-{repr}"),
                            },
                            inner.type_id,
                            unary.span,
                        );
                        return TirExpr::new(
                            TirExprKind::Cast {
                                expr: Box::new(neg_literal),
                                target_type: *target_type,
                            },
                            *target_type,
                            unary.span,
                        );
                    }
                }
                _ => {}
            }
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

        // Handle assignment to global variables
        if let TirExprKind::GlobalVarGet {
            module_source,
            name,
        } = &target.kind
        {
            // Check if the global is mutable (check both local and imported globals)
            let is_mutable = self
                .current_module_globals
                .get(name)
                .map(|(_, m)| *m)
                .or_else(|| {
                    // For imported globals, the name in the TIR is the original name from source
                    // We need to find it by iterating through imported_globals
                    self.imported_globals
                        .values()
                        .find(|(src, orig_name, _, _)| src == module_source && orig_name == name)
                        .map(|(_, _, _, m)| *m)
                });

            if let Some(is_mut) = is_mutable {
                if !is_mut {
                    self.errors.push(TypeError::CannotAssign {
                        message: format!("cannot assign to immutable global variable '{name}'"),
                        span: assign.target.span(),
                    });
                    return TirExpr::new(TirExprKind::Unit, TypeTable::ERROR, assign.span);
                }
                // Generate GlobalVarSet instead of Assign
                return TirExpr::new(
                    TirExprKind::GlobalVarSet {
                        module_source: module_source.clone(),
                        name: name.clone(),
                        value: Box::new(value.clone()),
                    },
                    value.type_id,
                    assign.span,
                );
            }
        }

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
                            // Each variant case has exactly one payload.
                            // Unit variants expect 0 args, non-unit variants expect 1 arg.
                            let payload_is_unit = matches!(
                                self.type_table.borrow().get(case_data.payload),
                                ResolvedType::Unit
                            );
                            let expected_args = usize::from(!payload_is_unit);

                            if args.len() != expected_args {
                                self.errors.push(TypeError::ArgumentCountMismatch {
                                    expected: expected_args,
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

                            // Payload is the single argument, or None for unit variants
                            let payload = args.into_iter().next().map(Box::new);

                            return TirExpr::new(
                                TirExprKind::VariantConstruct {
                                    variant_type,
                                    case_index: case_index as u32,
                                    case_name: case_data.name.clone(),
                                    payload,
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

        // For local function calls (empty module_path), use the current module source
        // to ensure DCE and codegen can find the function correctly
        let module_source = if module_path.is_empty() {
            self.current_module_source.clone()
        } else {
            ModuleSource::from_path(&module_path)
        };

        TirExpr::new(
            TirExprKind::Call {
                func: FunctionRef::External {
                    module_source,
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
                // Type aliases from WASI (e.g., Mark, Instant, Duration)
                _ => {
                    // First check if it's a registered newtype in type_aliases
                    if let Some(&newtype_id) = self.type_aliases.get(&named.name) {
                        return newtype_id;
                    }
                    // Otherwise, try to resolve via WASI registry's type aliases
                    let aliased = self.wasi_registry.get_type_alias(&named.name).cloned();
                    if let Some(aliased) = aliased {
                        // Create a newtype for this WASI type alias
                        let base_type = self.resolve_wasi_type(&aliased);
                        let newtype_id = self.type_table.borrow_mut().make_newtype(
                            named.name.clone(),
                            ModuleSource::wasi("clocks"),
                            base_type,
                        );
                        // Cache the newtype for future lookups
                        self.type_aliases.insert(named.name.clone(), newtype_id);
                        newtype_id
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

    /// Get the String struct type (from core:string)
    fn get_string_struct_type(&mut self) -> TypeId {
        self.type_table
            .borrow_mut()
            .make_struct("String".to_string(), ModuleSource::core("string"))
    }

    /// Build a `from_pair` call for i128/u128 large literal construction
    fn build_from_pair_call(
        &self,
        type_name: &str,
        low: u64,
        high: i64,
        target_type: TypeId,
        span: Span,
    ) -> TirExpr {
        let low_literal = TirExpr::new(
            TirExprKind::IntLiteral {
                value: low,
                repr: low.to_string(),
            },
            TypeTable::U64,
            span,
        );
        let high_literal = TirExpr::new(
            TirExprKind::IntLiteral {
                value: high as u64,
                repr: high.to_string(),
            },
            if type_name == "u128" {
                TypeTable::U64
            } else {
                TypeTable::I64
            },
            span,
        );

        let mangled_func_name = format!("{type_name}::from_pair");
        let method_info =
            LocalMethodName::new(type_name.to_string(), None, "from_pair".to_string());

        TirExpr::new(
            TirExprKind::StaticCall {
                func: FunctionRef::External {
                    module_source: ModuleSource::core("prelude/int128"),
                    name: mangled_func_name,
                    monomorph_info: None,
                    method_info: Some(method_info),
                },
                args: vec![low_literal, high_literal],
            },
            target_type,
            span,
        )
    }

    /// Get the return type of a builtin function
    ///
    /// Returns the pre-resolved `TypeId` from the `BuiltinRegistry`.
    /// For generic builtins like `array_new<T>`, returns a type containing
    /// `TypeParam` placeholders that get substituted during monomorphization.
    fn get_builtin_return_type(&self, name: &str) -> TypeId {
        self.builtin_registry
            .get_return_type(name)
            .unwrap_or(TypeTable::UNIT)
    }

    /// Look up function parameter types from callee expression
    fn lookup_function_param_types(&mut self, callee: &Expr) -> Vec<TypeId> {
        match callee {
            Expr::Ident(ident) => {
                // Check for qualified name (Type::method or Effect::operation)
                if let Some(pos) = ident.name.find("::") {
                    let prefix = &ident.name[..pos];
                    let suffix = &ident.name[pos + 2..];
                    // Check if it's a static method
                    if self.is_static_method(prefix, suffix) {
                        return self.lookup_static_method_param_types(prefix, suffix);
                    }
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
        // Handle numeric literal coercion
        if let Some(target_type) = expected_type {
            // Number literal coercion to integer
            if let Expr::Literal(lit) = expr
                && let Literal::Number(num_lit) = &lit.value
                && self.type_table.borrow().is_integer(target_type)
            {
                // Check if the literal can be an integer
                if Self::is_float_only_literal(&num_lit.repr) {
                    self.errors.push(TypeError::InvalidLiteral {
                        message: format!(
                            "cannot use float literal '{}' as integer (has decimal point or negative exponent)",
                            num_lit.repr
                        ),
                        span: lit.span,
                    });
                    return TirExpr::new(
                        TirExprKind::IntLiteral {
                            value: 0,
                            repr: num_lit.repr.clone(),
                        },
                        target_type,
                        lit.span,
                    );
                }
                match Self::parse_int_literal(&num_lit.repr) {
                    Ok(value) => {
                        return TirExpr::new(
                            TirExprKind::IntLiteral {
                                value,
                                repr: num_lit.repr.clone(),
                            },
                            target_type,
                            lit.span,
                        );
                    }
                    Err(message) => {
                        self.errors.push(TypeError::InvalidLiteral {
                            message,
                            span: lit.span,
                        });
                        return TirExpr::new(
                            TirExprKind::IntLiteral {
                                value: 0,
                                repr: num_lit.repr.clone(),
                            },
                            target_type,
                            lit.span,
                        );
                    }
                }
            }

            // Negated number literal coercion to integer: -42 as i64
            if let Expr::Unary(unary) = expr
                && unary.op == UnaryOp::Neg
                && let Expr::Literal(lit) = &unary.expr
                && let Literal::Number(num_lit) = &lit.value
                && self.type_table.borrow().is_integer(target_type)
            {
                // Check if the literal can be an integer
                if Self::is_float_only_literal(&num_lit.repr) {
                    self.errors.push(TypeError::InvalidLiteral {
                        message: format!(
                            "cannot use float literal '-{}' as integer (has decimal point or negative exponent)",
                            num_lit.repr
                        ),
                        span: unary.span,
                    });
                    return TirExpr::new(
                        TirExprKind::IntLiteral {
                            value: 0,
                            repr: format!("-{}", num_lit.repr),
                        },
                        target_type,
                        unary.span,
                    );
                }
                match Self::parse_int_literal(&num_lit.repr) {
                    Ok(value) => {
                        // Negate as i64 then store as u64 (two's complement)
                        let neg_value = (value as i64).wrapping_neg() as u64;
                        return TirExpr::new(
                            TirExprKind::IntLiteral {
                                value: neg_value,
                                repr: format!("-{}", num_lit.repr),
                            },
                            target_type,
                            unary.span,
                        );
                    }
                    Err(message) => {
                        self.errors.push(TypeError::InvalidLiteral {
                            message,
                            span: lit.span,
                        });
                        return TirExpr::new(
                            TirExprKind::IntLiteral {
                                value: 0,
                                repr: format!("-{}", num_lit.repr),
                            },
                            target_type,
                            unary.span,
                        );
                    }
                }
            }

            // Number literal coercion to float
            if let Expr::Literal(lit) = expr
                && let Literal::Number(num_lit) = &lit.value
                && self.type_table.borrow().is_float(target_type)
            {
                match Self::parse_float_literal(&num_lit.repr) {
                    Ok(value) => {
                        return TirExpr::new(
                            TirExprKind::FloatLiteral {
                                value,
                                repr: num_lit.repr.clone(),
                            },
                            target_type,
                            lit.span,
                        );
                    }
                    Err(message) => {
                        self.errors.push(TypeError::InvalidLiteral {
                            message,
                            span: lit.span,
                        });
                        return TirExpr::new(
                            TirExprKind::FloatLiteral {
                                value: 0.0,
                                repr: num_lit.repr.clone(),
                            },
                            target_type,
                            lit.span,
                        );
                    }
                }
            }

            // Negated number literal coercion to float: -3.14 as f32
            if let Expr::Unary(unary) = expr
                && unary.op == UnaryOp::Neg
                && let Expr::Literal(lit) = &unary.expr
                && let Literal::Number(num_lit) = &lit.value
                && self.type_table.borrow().is_float(target_type)
            {
                match Self::parse_float_literal(&num_lit.repr) {
                    Ok(value) => {
                        return TirExpr::new(
                            TirExprKind::FloatLiteral {
                                value: -value,
                                repr: format!("-{}", num_lit.repr),
                            },
                            target_type,
                            unary.span,
                        );
                    }
                    Err(message) => {
                        self.errors.push(TypeError::InvalidLiteral {
                            message,
                            span: lit.span,
                        });
                        return TirExpr::new(
                            TirExprKind::FloatLiteral {
                                value: 0.0,
                                repr: format!("-{}", num_lit.repr),
                            },
                            target_type,
                            unary.span,
                        );
                    }
                }
            }

            // i128/u128 literal coercion: let x: u128 = 42 → u128::from_u64(42 as u64)
            // For values larger than u64, use from_pair: let x: u128 = 340... → u128::from_pair(low, high)
            if let Expr::Literal(lit) = expr
                && let Literal::Number(num_lit) = &lit.value
                && !Self::is_float_only_literal(&num_lit.repr)
            {
                let struct_name = match self.type_table.borrow().get(target_type).clone() {
                    ResolvedType::Struct { name, .. } => Some(name),
                    _ => None,
                };

                if let Some(name) = struct_name
                    && (name == "u128" || name == "i128")
                {
                    // Try to parse the literal value as u64 first (most efficient path)
                    if let Ok(value) = Self::parse_int_literal(&num_lit.repr) {
                        // For u128: value fits in u64, use from_u64
                        // For i128: value must also fit in i64 (positive range)
                        let use_from_u64_or_i64 = if name == "u128" {
                            true // u64 always works for u128
                        } else {
                            // For i128, check if value fits in i64 positive range
                            i64::try_from(value).is_ok()
                        };

                        if use_from_u64_or_i64 {
                            let (inner_type, method_name) = if name == "u128" {
                                (TypeTable::U64, "from_u64")
                            } else {
                                (TypeTable::I64, "from_i64")
                            };

                            let inner_literal = TirExpr::new(
                                TirExprKind::IntLiteral {
                                    value,
                                    repr: num_lit.repr.clone(),
                                },
                                inner_type,
                                lit.span,
                            );

                            // Build static call: u128::from_u64(value) or i128::from_i64(value)
                            let mangled_func_name = format!("{name}::{method_name}");
                            let method_info =
                                LocalMethodName::new(name.clone(), None, method_name.to_string());

                            return TirExpr::new(
                                TirExprKind::StaticCall {
                                    func: FunctionRef::External {
                                        module_source: ModuleSource::core("prelude/int128"),
                                        name: mangled_func_name,
                                        monomorph_info: None,
                                        method_info: Some(method_info),
                                    },
                                    args: vec![inner_literal],
                                },
                                target_type,
                                lit.span,
                            );
                        }
                    }

                    // Value doesn't fit in u64 (or i64 for i128), use from_pair
                    // Parse at compile time and generate from_pair(low, high)
                    if name == "u128" {
                        if let Ok(value) = Self::parse_u128_literal(&num_lit.repr) {
                            let (low, high) = Self::unpack_u128(value);
                            return self.build_from_pair_call(
                                &name,
                                low,
                                high as i64,
                                target_type,
                                lit.span,
                            );
                        }
                        self.errors.push(TypeError::InvalidLiteral {
                            message: format!("invalid u128 literal: {}", num_lit.repr),
                            span: lit.span,
                        });
                    } else {
                        // i128: parse as i128 (handles positive values > i64::MAX)
                        if let Ok(value) = Self::parse_i128_literal(&num_lit.repr) {
                            let (low, high) = Self::unpack_i128(value);
                            return self.build_from_pair_call(
                                &name,
                                low,
                                high,
                                target_type,
                                lit.span,
                            );
                        }
                        self.errors.push(TypeError::InvalidLiteral {
                            message: format!("invalid i128 literal: {}", num_lit.repr),
                            span: lit.span,
                        });
                    }
                }
            }

            // Handle negated number literal to i128: let x: i128 = -100
            // For large values: let x: i128 = -170... → i128::from_pair(low, high)
            if let Expr::Unary(unary) = expr
                && unary.op == ast::UnaryOp::Neg
                && let Expr::Literal(lit) = &unary.expr
                && let Literal::Number(num_lit) = &lit.value
                && !Self::is_float_only_literal(&num_lit.repr)
            {
                let struct_name = match self.type_table.borrow().get(target_type).clone() {
                    ResolvedType::Struct { name, .. } => Some(name),
                    _ => None,
                };

                if let Some(name) = struct_name
                    && name == "i128"
                {
                    // Parse the negated value directly using Rust's i128
                    let negated_repr = format!("-{}", Self::clean_literal_repr(&num_lit.repr));
                    if let Ok(value) = Self::parse_i128_literal(&negated_repr) {
                        let (low, high) = Self::unpack_i128(value);
                        return self.build_from_pair_call(
                            &name,
                            low,
                            high,
                            target_type,
                            unary.span,
                        );
                    }
                    self.errors.push(TypeError::InvalidLiteral {
                        message: format!("invalid i128 literal: -{}", num_lit.repr),
                        span: unary.span,
                    });
                }
            }
        }

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
                    // Pass the element type as expected type for recursive coercion
                    self.resolve_expr_with_expected_type(elem, ctx, Some(element_type))
                })
                .collect();

            return TirExpr::new(
                TirExprKind::ArrayLiteral { elements },
                target_type,
                expr.span(),
            );
        }

        // Handle match expression coercion - propagate expected type to arm bodies
        if let Some(target_type) = expected_type
            && let Expr::Match(match_expr) = expr
        {
            return self.resolve_match_expr_with_expected_type(match_expr, ctx, target_type);
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

        // Get struct name and module source from base type
        // The struct_module_source is where the struct is defined (and inherent methods live)
        let (struct_name, module_path, struct_module_source) =
            match self.type_table.borrow().get(base_type_id) {
                ResolvedType::Struct {
                    name,
                    module_source,
                    ..
                } => (
                    name.clone(),
                    module_source.to_path(),
                    Some(module_source.clone()),
                ),
                ResolvedType::GenericInstance {
                    name,
                    module_source,
                    ..
                } => (
                    name.clone(),
                    module_source.to_path(),
                    Some(module_source.clone()),
                ),
                // Primitive types have impl blocks in core:prelude/primitives
                ResolvedType::Primitive(_) => {
                    let prim_module = ModuleSource::Core {
                        name: "prelude/primitives".to_string(),
                    };
                    (
                        self.type_table.borrow().mangle_type_name(base_type_id),
                        prim_module.to_path(),
                        Some(prim_module),
                    )
                }
                // BuiltinArray is Array - impl blocks are in core:prelude
                ResolvedType::BuiltinArray(_) => {
                    let prelude_module = ModuleSource::Core {
                        name: "prelude".to_string(),
                    };
                    (
                        "Array".to_string(),
                        prelude_module.to_path(),
                        Some(prelude_module),
                    )
                }
                _ => (
                    self.type_table.borrow().mangle_type_name(base_type_id),
                    vec![],
                    None,
                ),
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
                // BuiltinArray(elem) is Array<elem>, so type args = [elem]
                ResolvedType::BuiltinArray(elem) => Some(vec![elem]),
                _ => None,
            };

        // If inherent method not found, try trait methods
        // Track the module source where the trait impl was found
        let mut trait_impl_module_source: Option<ModuleSource> = None;
        if method_info.is_none()
            && let Some((found_trait, info, impl_source)) = self.find_trait_method_for_type(
                &struct_name,
                &method_call.method,
                &module_path,
                receiver_type_args_for_trait.as_deref(),
            )
        {
            trait_name = Some(found_trait);
            method_info = Some(info);
            trait_impl_module_source = Some(impl_source);
        }

        // Get method info (error if method not found)
        let MethodInfo {
            mut return_type,
            self_kind,
            param_types,
            inherited_from_base,
        } = if let Some(info) = method_info {
            info
        } else {
            let type_name = self.type_table.borrow().type_name(base_type_id);
            self.errors.push(TypeError::TypeMismatch {
                expected: format!(
                    "type '{}' to have method '{}'",
                    type_name, method_call.method
                ),
                found: format!("no method '{}' found", method_call.method),
                span: method_call.span,
            });
            // Default to Unknown type for error recovery
            MethodInfo {
                return_type: TypeTable::UNKNOWN,
                self_kind: ast::SelfKind::Ref,
                param_types: vec![],
                inherited_from_base: None,
            }
        };

        // Type check method arguments against expected parameter types (newtype-aware)
        // If method was inherited from a newtype's base type, substitute base->newtype in params
        let expected_param_types: Vec<TypeId> = if let Some(base_type_id) = inherited_from_base {
            // Get the newtype that the method is being called on
            let newtype_id = self.get_base_type(receiver.type_id);
            // Substitute base type with newtype in all parameter types
            param_types
                .iter()
                .map(|&ty| self.substitute_newtype_in_type(ty, base_type_id, newtype_id))
                .collect()
        } else {
            param_types
        };

        // Check each argument against expected parameter type
        for (i, (arg, &expected_type)) in args.iter().zip(expected_param_types.iter()).enumerate() {
            if let Some((expected_name, actual_name)) =
                self.check_newtype_arg_mismatch(arg.type_id, expected_type)
            {
                self.errors.push(TypeError::TypeMismatch {
                    expected: format!("argument {} to be {}", i + 1, expected_name),
                    found: actual_name,
                    span: method_call
                        .args
                        .get(i)
                        .map_or(method_call.span, super::ast::Expr::span),
                });
            }
        }

        // Substitute return type for inherited newtype methods
        // e.g., Point::clone_point() -> Point becomes Location::clone_point() -> Location
        if let Some(base_type_id) = inherited_from_base {
            let newtype_id = self.get_base_type(receiver.type_id);
            return_type = self.substitute_newtype_in_type(return_type, base_type_id, newtype_id);
        }

        // Adjust receiver based on what the method expects (self_kind)
        receiver = self.adjust_receiver_for_self_kind(receiver, self_kind, method_call.span);

        // Build unified substitution context for double generics
        // Type param indices are assigned as follows:
        // - Impl type params (from struct): 0, 1, 2, ...
        // - Method type params: offset, offset+1, ... (where offset = impl_type_params.len())
        let mut subst_ctx = SubstitutionContext::new();
        let mut impl_offset = 0u32;

        // First, add impl-level type args from receiver's generic type (use base type)
        // IMPORTANT: Skip this for trait methods because find_trait_method_for_type already
        // resolved the return type using associated type bindings. Adding impl_args here would
        // incorrectly substitute TypeParams from the OUTER context (e.g., TreeMap's K, V) that
        // happen to have the same indices as this impl's type params (e.g., Array's T).
        if trait_name.is_none() {
            match self.type_table.borrow().get(base_type_id).clone() {
                ResolvedType::GenericInstance {
                    type_args: receiver_type_args,
                    ..
                } if !receiver_type_args.is_empty() => {
                    impl_offset = receiver_type_args.len() as u32;
                    subst_ctx = subst_ctx.with_impl_args(&receiver_type_args);
                }
                // BuiltinArray(elem) is Array<elem>, so type args = [elem]
                ResolvedType::BuiltinArray(elem) => {
                    impl_offset = 1;
                    subst_ctx = subst_ctx.with_impl_args(&[elem]);
                }
                _ => {}
            }
        } else {
            // For trait methods, just compute impl_offset for method type args
            match self.type_table.borrow().get(base_type_id).clone() {
                ResolvedType::GenericInstance { type_args, .. } if !type_args.is_empty() => {
                    impl_offset = type_args.len() as u32;
                }
                ResolvedType::BuiltinArray(_) => {
                    impl_offset = 1;
                }
                _ => {}
            }
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
        let (receiver_struct_name, base_struct_name, impl_type_arg_names, receiver_type_args) =
            match self.type_table.borrow().get(base_type_id).clone() {
                ResolvedType::GenericInstance {
                    name, type_args, ..
                } => {
                    let type_arg_names: Vec<String> = type_args
                        .iter()
                        .map(|t| self.type_table.borrow().mangle_type_name(*t))
                        .collect();
                    let mangled = format!("{}<{}>", name, type_arg_names.join(","));
                    (
                        mangled,
                        name.clone(),
                        type_arg_names,
                        Some(type_args.clone()),
                    )
                }
                ResolvedType::BuiltinArray(elem) => {
                    let elem_name = self.type_table.borrow().mangle_type_name(elem);
                    let _mangled = format!("Array<{elem_name}>");
                    (
                        "Array".to_string(),
                        "Array".to_string(),
                        vec![elem_name],
                        Some(vec![elem]),
                    )
                }
                _ => {
                    let name = self.type_table.borrow().mangle_type_name(base_type_id);
                    (name.clone(), name, vec![], None)
                }
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
        let monomorph_info = receiver_type_args.map(|type_args| {
            let generic_name = format!("{}::{}", base_struct_name, method_call.method);
            MonomorphInfo {
                generic_name,
                type_args,
            }
        });

        // Convert method type args to string names for method_info
        // Use inferred type args if available, otherwise use explicit type args
        let method_type_arg_names: Vec<String> = method_type_args
            .iter()
            .map(|t| self.type_table.borrow().mangle_type_name(*t))
            .collect();

        // Build method_info with base struct name, then apply impl and method type args
        let method_info = LocalMethodName::new(
            base_struct_name, // Use base struct name without type params
            trait_name,
            method_call.method.clone(),
        )
        .with_type_args(&impl_type_arg_names, &method_type_arg_names);

        // Use trait impl module source if this is a trait method,
        // otherwise use the struct's module (where inherent methods are defined)
        let method_module_source = trait_impl_module_source
            .or(struct_module_source)
            .unwrap_or_else(|| self.current_module_source.clone());

        TirExpr::new(
            TirExprKind::MethodCall {
                receiver: Box::new(receiver),
                func: FunctionRef::External {
                    module_source: method_module_source,
                    name: mangled_method_name,
                    monomorph_info,
                    method_info: Some(method_info),
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
        // Resolve the target type first to get struct name for parameter type lookup
        let target_type_id = self.resolve_type(&static_call.target_type);

        // Extract struct name for parameter type lookup (follow newtypes to base)
        let struct_name_for_lookup = {
            let mut current_type = target_type_id;
            loop {
                match self.type_table.borrow().get(current_type).clone() {
                    ResolvedType::Struct { name, .. } => break Some(name),
                    ResolvedType::GenericInstance { name, .. } => break Some(name),
                    ResolvedType::Newtype { base_type, .. } => current_type = base_type,
                    _ => break None,
                }
            }
        };

        // Look up parameter types for coercion
        let param_types = struct_name_for_lookup
            .as_ref()
            .map(|name| self.lookup_static_method_param_types(name, &static_call.method))
            .unwrap_or_default();

        // Resolve arguments with expected types for coercion
        let args: Vec<TirExpr> = static_call
            .args
            .iter()
            .enumerate()
            .map(|(i, a)| {
                let expected_type = param_types.get(i).copied();
                self.resolve_expr_with_expected_type(a, ctx, expected_type)
            })
            .collect();

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
                    // Each variant case has exactly one payload.
                    let payload_is_unit = matches!(
                        self.type_table.borrow().get(case_data.payload),
                        ResolvedType::Unit
                    );
                    let expected_args = usize::from(!payload_is_unit);

                    if args.len() != expected_args {
                        self.errors.push(TypeError::ArgumentCountMismatch {
                            expected: expected_args,
                            found: args.len(),
                            span: static_call.span,
                        });
                        return TirExpr::new(TirExprKind::Unit, TypeTable::ERROR, static_call.span);
                    }

                    // Payload is the single argument, or None for unit variants
                    let payload = args.into_iter().next().map(Box::new);

                    // Create VariantConstruct expression
                    return TirExpr::new(
                        TirExprKind::VariantConstruct {
                            variant_type: target_type_id,
                            case_index: case_index as u32,
                            case_name: case_data.name.clone(),
                            payload,
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

        // Handle generic variant construction: Result::<i32, String>::Ok(42)
        if let ResolvedType::GenericInstance {
            name,
            module_source: _,
            type_args: _,
        } = self.type_table.borrow().get(target_type_id).clone()
        {
            // Check if the base type is a variant
            if let Some(variant_info) = self.variant_cases.get(&name).cloned() {
                // This is a generic variant like Result<T, E>
                // Find the case by name
                if let Some((case_index, case_data)) = variant_info
                    .cases
                    .iter()
                    .enumerate()
                    .find(|(_, c)| c.name == static_call.method)
                {
                    // Each variant case has exactly one payload.
                    let payload_is_unit = matches!(
                        self.type_table.borrow().get(case_data.payload),
                        ResolvedType::Unit
                    );
                    let expected_args = usize::from(!payload_is_unit);

                    if args.len() != expected_args {
                        self.errors.push(TypeError::ArgumentCountMismatch {
                            expected: expected_args,
                            found: args.len(),
                            span: static_call.span,
                        });
                        return TirExpr::new(TirExprKind::Unit, TypeTable::ERROR, static_call.span);
                    }

                    // Payload is the single argument, or None for unit variants
                    let payload = args.into_iter().next().map(Box::new);

                    // Create VariantConstruct expression
                    return TirExpr::new(
                        TirExprKind::VariantConstruct {
                            variant_type: target_type_id,
                            case_index: case_index as u32,
                            case_name: case_data.name.clone(),
                            payload,
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
            }
        }

        let (struct_name, module_path, mangled_struct_name, struct_type_args) = match self
            .type_table
            .borrow()
            .get(target_type_id)
        {
            ResolvedType::Struct {
                name,
                module_source,
                ..
            } => (name.clone(), module_source.to_path(), name.clone(), vec![]),
            ResolvedType::Resource {
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
                    .map(|t| self.type_table.borrow().mangle_type_name(*t))
                    .collect();
                let mangled = format!("{}<{}>", name, type_arg_names.join(","));
                (
                    name.clone(),
                    module_source.to_path(),
                    mangled,
                    type_args.clone(),
                )
            }
            ResolvedType::Newtype { base_type, .. } => {
                // For newtypes, look through to the base type for static method lookup
                match self.type_table.borrow().get(*base_type).clone() {
                    ResolvedType::Struct {
                        name,
                        module_source,
                        ..
                    } => (name.clone(), module_source.to_path(), name.clone(), vec![]),
                    ResolvedType::GenericInstance {
                        name,
                        module_source,
                        type_args,
                    } => {
                        let type_arg_names: Vec<String> = type_args
                            .iter()
                            .map(|t| self.type_table.borrow().mangle_type_name(*t))
                            .collect();
                        let mangled = format!("{}<{}>", name, type_arg_names.join(","));
                        (
                            name.clone(),
                            module_source.to_path(),
                            mangled,
                            type_args.clone(),
                        )
                    }
                    // Handle chained newtypes recursively
                    ResolvedType::Newtype {
                        base_type: inner_base,
                        ..
                    } => {
                        // Follow the chain to find the ultimate struct
                        let mut current = inner_base;
                        loop {
                            match self.type_table.borrow().get(current).clone() {
                                ResolvedType::Struct {
                                    name,
                                    module_source,
                                    ..
                                } => {
                                    break (
                                        name.clone(),
                                        module_source.to_path(),
                                        name.clone(),
                                        vec![],
                                    );
                                }
                                ResolvedType::Newtype {
                                    base_type: next, ..
                                } => current = next,
                                _ => {
                                    return TirExpr::new(
                                        TirExprKind::Unit,
                                        TypeTable::ERROR,
                                        static_call.span,
                                    );
                                }
                            }
                        }
                    }
                    _ => {
                        return TirExpr::new(TirExprKind::Unit, TypeTable::ERROR, static_call.span);
                    }
                }
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
        let (monomorph_info, impl_type_arg_names): (Option<MonomorphInfo>, Vec<String>) =
            if struct_type_args.is_empty() {
                (None, vec![])
            } else {
                // Generic static method: track the original generic name
                let generic_name = format!("{}::{}", struct_name, static_call.method);
                let type_arg_names: Vec<String> = struct_type_args
                    .iter()
                    .map(|t| self.type_table.borrow().mangle_type_name(*t))
                    .collect();
                (
                    Some(MonomorphInfo {
                        generic_name,
                        type_args: struct_type_args,
                    }),
                    type_arg_names,
                )
            };

        // Build method_info with base struct name, then apply type args
        let method_info = LocalMethodName::new(
            struct_name, // Use base struct name without type params
            None,        // Static methods are inherent, no trait
            static_call.method.clone(),
        )
        .with_struct_type_args(&impl_type_arg_names);

        TirExpr::new(
            TirExprKind::StaticCall {
                func: FunctionRef::External {
                    module_source: ModuleSource::from_path(&module_path),
                    name: mangled_func_name,
                    monomorph_info,
                    method_info: Some(method_info),
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
                // Check impl blocks
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

                // Check resource declarations
                if let Item::Resource(resource) = item
                    && resource.name == struct_name
                {
                    for method in &resource.methods {
                        // Static methods have no self parameter (no &TcpSocket or &Self)
                        let has_self = method.params.iter().any(|p| {
                                matches!(&p.ty, ast::Type::Reference(r) | ast::Type::MutReference(r)
                                    if matches!(&**r, ast::Type::Named(n) if n.name == "Self" || n.name == struct_name))
                                    || matches!(&p.ty, ast::Type::Named(n) if n.name == "Self" || n.name == struct_name)
                            });
                        if method.name == method_name && !has_self {
                            return method
                                .return_type
                                .as_ref()
                                .map(|t| self.resolve_type(t))
                                .unwrap_or(TypeTable::UNIT);
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

        // Search resource declarations in all modules
        for module in self.loaded_modules.values() {
            for item in &module.items {
                if let Item::Resource(resource) = item
                    && resource.name == struct_name
                {
                    // Find the method in the resource
                    for method in &resource.methods {
                        // Static methods have no self parameter
                        let has_self = method.params.iter().any(|p| {
                                matches!(&p.ty, ast::Type::Reference(r) | ast::Type::MutReference(r)
                                    if matches!(&**r, ast::Type::Named(n) if n.name == "Self" || n.name == struct_name))
                                    || matches!(&p.ty, ast::Type::Named(n) if n.name == "Self" || n.name == struct_name)
                            });
                        if method.name == method_name && !has_self {
                            return method
                                .return_type
                                .as_ref()
                                .map(|t| self.resolve_type(t))
                                .unwrap_or(TypeTable::UNIT);
                        }
                    }
                }
            }
        }

        TypeTable::UNKNOWN
    }

    /// Look up static method parameter types for coercion
    fn lookup_static_method_param_types(
        &mut self,
        struct_name: &str,
        method_name: &str,
    ) -> Vec<TypeId> {
        // Check in current module's impl blocks
        let params: Option<Vec<_>> = self.current_module_items.iter().find_map(|item| {
            if let Item::Impl(impl_block) = item {
                let impl_struct_name = self.get_type_name(&impl_block.ty);
                if impl_struct_name == struct_name {
                    for method in &impl_block.methods {
                        let has_self = method
                            .params
                            .iter()
                            .any(|p| p.self_kind != ast::SelfKind::None);
                        if method.name == method_name && !has_self {
                            return Some(method.params.clone());
                        }
                    }
                }
            }
            None
        });

        if let Some(params) = params {
            return params.iter().map(|p| self.resolve_type(&p.ty)).collect();
        }

        // Check loaded modules' impl blocks
        for module in self.loaded_modules.values() {
            let params: Option<Vec<_>> = module.items.iter().find_map(|item| {
                if let Item::Impl(impl_block) = item {
                    let impl_struct_name = self.get_type_name(&impl_block.ty);
                    if impl_struct_name == struct_name {
                        for method in &impl_block.methods {
                            let has_self = method
                                .params
                                .iter()
                                .any(|p| p.self_kind != ast::SelfKind::None);
                            if method.name == method_name && !has_self {
                                return Some(method.params.clone());
                            }
                        }
                    }
                }
                None
            });

            if let Some(params) = params {
                return params.iter().map(|p| self.resolve_type(&p.ty)).collect();
            }
        }

        Vec::new()
    }

    /// Check if an expression is a numeric literal
    fn is_numeric_literal(&self, expr: &Expr) -> bool {
        matches!(
            expr,
            Expr::Literal(lit) if matches!(lit.value, Literal::Number(_))
        )
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

        // Check resource declarations in loaded modules
        for module in self.loaded_modules.values() {
            for item in &module.items {
                if let Item::Resource(resource) = item
                    && resource.name == struct_name
                {
                    for method in &resource.methods {
                        // Static methods have no self parameter
                        let has_self = method.params.iter().any(|p| {
                                matches!(&p.ty, ast::Type::Reference(r) | ast::Type::MutReference(r)
                                    if matches!(&**r, ast::Type::Named(n) if n.name == "Self" || n.name == struct_name))
                                    || matches!(&p.ty, ast::Type::Named(n) if n.name == "Self" || n.name == struct_name)
                            });
                        if method.name == method_name && !has_self {
                            return true;
                        }
                    }
                }
            }
        }

        // For newtypes, check if the base type has the static method
        if let Some(&newtype_id) = self.type_aliases.get(struct_name)
            && let ResolvedType::Newtype { base_type, .. } =
                self.type_table.borrow().get(newtype_id).clone()
        {
            // Get the base type's name and recursively check
            let base_name = self.type_table.borrow().type_name(base_type);
            if self.is_static_method(&base_name, method_name) {
                return true;
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
        // For newtypes, resolve to the base type's static method
        let (actual_struct_name, actual_mangled_name) =
            if let Some(&newtype_id) = self.type_aliases.get(struct_name) {
                if let ResolvedType::Newtype { base_type, .. } =
                    self.type_table.borrow().get(newtype_id).clone()
                {
                    // Follow the chain to find the ultimate struct
                    let base_name = self.get_ultimate_base_struct_name(base_type);
                    let mangled = format!("{base_name}::{method_name}");
                    (base_name, mangled)
                } else {
                    (struct_name.to_string(), mangled_func_name.to_string())
                }
            } else {
                (struct_name.to_string(), mangled_func_name.to_string())
            };

        // Look up return type using the actual struct name
        let return_type = self.lookup_static_method_return_type(
            &actual_struct_name,
            &[], // Module path will be looked up during lookup
            method_name,
            &actual_mangled_name,
        );

        // Determine module source for the actual struct
        let module_source = self.find_struct_module_source(&actual_struct_name);

        TirExpr::new(
            TirExprKind::StaticCall {
                func: FunctionRef::External {
                    module_source,
                    name: actual_mangled_name,
                    monomorph_info: None,
                    method_info: Some(LocalMethodName::new(
                        actual_struct_name,
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

    /// Get the ultimate base struct name following the newtype chain
    fn get_ultimate_base_struct_name(&self, type_id: TypeId) -> String {
        let mut current = type_id;
        loop {
            match self.type_table.borrow().get(current).clone() {
                ResolvedType::Struct { name, .. } => return name,
                ResolvedType::GenericInstance { name, .. } => return name,
                ResolvedType::Newtype { base_type, .. } => current = base_type,
                _ => return self.type_table.borrow().type_name(current),
            }
        }
    }

    /// Find the module source for a struct by name
    fn find_struct_module_source(&self, struct_name: &str) -> ModuleSource {
        // Check current module
        for item in &self.current_module_items {
            match item {
                Item::Struct(s) if s.name == struct_name => {
                    return self.current_module_source.clone();
                }
                Item::Resource(r) if r.name == struct_name => {
                    return self.current_module_source.clone();
                }
                _ => {}
            }
        }

        // Check loaded modules
        for (path, module) in self.loaded_modules {
            for item in &module.items {
                match item {
                    Item::Struct(s) if s.name == struct_name => {
                        return ModuleSource::from_path(path);
                    }
                    Item::Resource(r) if r.name == struct_name => {
                        return ModuleSource::from_path(path);
                    }
                    _ => {}
                }
            }
        }

        // Default to current module source
        self.current_module_source.clone()
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
        let (struct_name, module_path, receiver_type_args, newtype_base) = match &base_type {
            ResolvedType::Struct {
                name,
                module_source,
                ..
            } => (name.clone(), module_source.to_path(), None, None),
            // Resource types use reference semantics - handle like struct for method lookup
            ResolvedType::Resource {
                name,
                module_source,
            } => (name.clone(), module_source.to_path(), None, None),
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
                None,
            ),
            // Newtype: first try looking up methods on the newtype itself,
            // then fall back to the base type for method inheritance
            ResolvedType::Newtype {
                name,
                module_source,
                base_type,
            } => (
                name.clone(),
                module_source.to_path(),
                None,
                Some(*base_type),
            ),
            // Primitive types - search for impl blocks in loaded modules
            // (e.g., impl i32 { fn to_string(&self) -> String { ... } })
            ResolvedType::Primitive(prim) => {
                // Use empty module path to trigger "search all loaded modules" logic
                (prim.as_str().to_string(), Vec::new(), None, None)
            }
            _ => return None,
        };

        // Build the mangled method name and look it up locally first
        let mangled_name = format!("{struct_name}::{method_name}");
        if let Some(&return_type) = self.function_return_types.get(&mangled_name) {
            // For locally registered methods, find self_kind and param_types from the AST
            if let Some((self_kind, param_types)) =
                self.find_local_method_info(&struct_name, method_name)
            {
                return Some(MethodInfo {
                    return_type,
                    self_kind,
                    param_types,
                    inherited_from_base: None,
                });
            }
            // Fallback: assume &self for methods (most common case), no param_types
            return Some(MethodInfo {
                return_type,
                self_kind: ast::SelfKind::Ref,
                param_types: vec![],
                inherited_from_base: None,
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
                                let param_types = self.extract_param_types(&method.params);

                                self.current_type_params = old_type_params;

                                return Some(MethodInfo {
                                    return_type,
                                    self_kind,
                                    param_types,
                                    inherited_from_base: None,
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
                                    let param_types = self.extract_param_types(&method.params);

                                    self.current_type_params = old_type_params;

                                    return Some(MethodInfo {
                                        return_type,
                                        self_kind,
                                        param_types,
                                        inherited_from_base: None,
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }

        // Search resource declarations in loaded modules for instance methods
        // Resource methods have &self or &mut self parameter (first param is reference to resource type)
        if !module_path.is_empty()
            && let Some(module) = self.loaded_modules.get(&module_path)
        {
            for item in &module.items {
                if let Item::Resource(resource) = item
                    && resource.name == struct_name
                {
                    for method in &resource.methods {
                        if method.name == method_name {
                            // Check if this is an instance method (has self parameter)
                            let has_self = method.params.iter().any(|p| {
                                matches!(&p.ty, ast::Type::Reference(r) | ast::Type::MutReference(r)
                                    if matches!(&**r, ast::Type::Named(n) if n.name == "Self" || n.name == struct_name))
                                    || matches!(&p.ty, ast::Type::Named(n) if n.name == "Self" || n.name == struct_name)
                            });
                            if has_self {
                                let return_type = method
                                    .return_type
                                    .as_ref()
                                    .map(|t| self.resolve_type(t))
                                    .unwrap_or(TypeTable::UNIT);
                                let param_types = self.extract_param_types(&method.params);
                                // Resource instance methods use &self (Ref) by default
                                return Some(MethodInfo {
                                    return_type,
                                    self_kind: ast::SelfKind::Ref,
                                    param_types,
                                    inherited_from_base: None,
                                });
                            }
                        }
                    }
                }
            }
        }

        // Also search all modules for resources if module_path is empty
        if module_path.is_empty() {
            for module in self.loaded_modules.values() {
                for item in &module.items {
                    if let Item::Resource(resource) = item
                        && resource.name == struct_name
                    {
                        for method in &resource.methods {
                            if method.name == method_name {
                                // Check if this is an instance method (has self parameter)
                                let has_self = method.params.iter().any(|p| {
                                    matches!(&p.ty, ast::Type::Reference(r) | ast::Type::MutReference(r)
                                        if matches!(&**r, ast::Type::Named(n) if n.name == "Self" || n.name == struct_name))
                                        || matches!(&p.ty, ast::Type::Named(n) if n.name == "Self" || n.name == struct_name)
                                });
                                if has_self {
                                    let return_type = method
                                        .return_type
                                        .as_ref()
                                        .map(|t| self.resolve_type(t))
                                        .unwrap_or(TypeTable::UNIT);
                                    let param_types = self.extract_param_types(&method.params);
                                    // Resource instance methods use &self (Ref) by default
                                    return Some(MethodInfo {
                                        return_type,
                                        self_kind: ast::SelfKind::Ref,
                                        param_types,
                                        inherited_from_base: None,
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }

        // For newtypes: if method not found on the newtype itself, try the base type
        // This enables method inheritance: Location (newtype of Point) can use Point's methods
        if let Some(base_type_id) = newtype_base {
            if let Some(mut method_info) = self.lookup_method_info(base_type_id, method_name) {
                // Mark that this method was inherited from the base type
                // This enables proper type checking (e.g., Point::add expects &Point,
                // but when called on Location, it should expect &Location)
                // Only set if not already set (for chained newtypes like C -> B -> A -> Point,
                // we want to keep the innermost base type where the method is defined)
                if method_info.inherited_from_base.is_none() {
                    method_info.inherited_from_base = Some(base_type_id);
                }
                return Some(method_info);
            }
            return None;
        }

        None
    }

    /// Find the method info (`self_kind` and `param_types`) for a method in current module items
    fn find_local_method_info(
        &mut self,
        struct_name: &str,
        method_name: &str,
    ) -> Option<(ast::SelfKind, Vec<TypeId>)> {
        // First collect method info without resolving types
        let mut found_method: Option<(ast::SelfKind, Vec<ast::Type>)> = None;

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
                            let self_kind = method
                                .params
                                .first()
                                .map(|p| p.self_kind)
                                .unwrap_or(ast::SelfKind::None);
                            // Extract non-self parameter types
                            let param_types: Vec<ast::Type> = method
                                .params
                                .iter()
                                .filter(|p| p.name != "self")
                                .map(|p| p.ty.clone())
                                .collect();
                            found_method = Some((self_kind, param_types));
                            break;
                        }
                    }
                }
            }
            if found_method.is_some() {
                break;
            }
        }

        // Now resolve the types (needs mutable borrow)
        found_method.map(|(self_kind, param_types_ast)| {
            let param_types: Vec<TypeId> = param_types_ast
                .iter()
                .map(|ty| self.resolve_type(ty))
                .collect();
            (self_kind, param_types)
        })
    }

    /// Extract parameter types (excluding self) from method parameters
    fn extract_param_types(&mut self, params: &[ast::Param]) -> Vec<TypeId> {
        params
            .iter()
            .filter(|p| p.name != "self")
            .map(|p| self.resolve_type(&p.ty))
            .collect()
    }

    /// Substitute a base type with a newtype in a type (handles references)
    /// For example: if `base_type` is Point and newtype is Location:
    ///   - Point -> Location
    ///   - &Point -> &Location
    ///   - &mut Point -> &mut Location
    fn substitute_newtype_in_type(
        &mut self,
        type_id: TypeId,
        base_type: TypeId,
        newtype: TypeId,
    ) -> TypeId {
        let ty = self.type_table.borrow().get(type_id).clone();
        match ty {
            // Direct match: base type -> newtype
            _ if type_id == base_type => newtype,

            // Reference: substitute the inner type
            ResolvedType::Ref(inner) => {
                let new_inner = self.substitute_newtype_in_type(inner, base_type, newtype);
                if new_inner == inner {
                    type_id
                } else {
                    self.type_table
                        .borrow_mut()
                        .intern(ResolvedType::Ref(new_inner))
                }
            }
            ResolvedType::MutRef(inner) => {
                let new_inner = self.substitute_newtype_in_type(inner, base_type, newtype);
                if new_inner == inner {
                    type_id
                } else {
                    self.type_table
                        .borrow_mut()
                        .intern(ResolvedType::MutRef(new_inner))
                }
            }

            // Other types: no substitution
            _ => type_id,
        }
    }

    /// Check if actual argument type matches expected parameter type (newtype-aware)
    /// Returns true if there's a mismatch involving newtypes
    fn check_newtype_arg_mismatch(
        &self,
        actual: TypeId,
        expected: TypeId,
    ) -> Option<(String, String)> {
        if actual == expected {
            return None;
        }

        let type_table = self.type_table.borrow();

        // Unwrap references to get the inner types
        let actual_inner = match type_table.get(actual) {
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => *inner,
            _ => actual,
        };
        let expected_inner = match type_table.get(expected) {
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => *inner,
            _ => expected,
        };

        // Check if either inner type is a newtype
        let actual_is_newtype =
            matches!(type_table.get(actual_inner), ResolvedType::Newtype { .. });
        let expected_is_newtype =
            matches!(type_table.get(expected_inner), ResolvedType::Newtype { .. });

        // If either is a newtype and they're different, that's a mismatch
        if (actual_is_newtype || expected_is_newtype) && actual_inner != expected_inner {
            let actual_name = type_table.type_name(actual);
            let expected_name = type_table.type_name(expected);
            return Some((expected_name, actual_name));
        }

        // Also check if one is the base type of the other
        if let ResolvedType::Newtype { base_type, .. } = type_table.get(actual_inner)
            && *base_type == expected_inner
        {
            let actual_name = type_table.type_name(actual);
            let expected_name = type_table.type_name(expected);
            return Some((expected_name, actual_name));
        }
        if let ResolvedType::Newtype { base_type, .. } = type_table.get(expected_inner)
            && *base_type == actual_inner
        {
            let actual_name = type_table.type_name(actual);
            let expected_name = type_table.type_name(expected);
            return Some((expected_name, actual_name));
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
                ..
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
            && let Some(module) = self.loaded_modules.get(&module_path)
        {
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
                            let arg_type = self
                                .type_table
                                .borrow()
                                .get(args[param_idx].type_id)
                                .clone();
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

    /// Get the ultimate base type by stripping all Ref/MutRef and Newtype wrappers
    /// This follows the entire chain: Ref(Newtype(Ref(Primitive))) -> Primitive
    fn get_ultimate_base_type(&self, type_id: TypeId) -> TypeId {
        let mut current = type_id;
        loop {
            match self.type_table.borrow().get(current).clone() {
                ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
                    current = inner;
                }
                ResolvedType::Newtype { base_type, .. } => {
                    current = base_type;
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
            ast::SelfKind::None | ast::SelfKind::Value => {
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
    /// Returns (`trait_name`, `MethodInfo`, `ModuleSource`) if found, None otherwise.
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
    ) -> Option<(String, MethodInfo, ModuleSource)> {
        let mut found_traits: Vec<(String, MethodInfo, ModuleSource)> = Vec::new();

        // Collect impl blocks to check (avoiding borrow issues)
        // Include associated type bindings for resolving Self::* types
        // Also track which module source each impl comes from
        let mut impl_blocks_to_check: Vec<(
            Type,
            Type,
            Vec<Function>,
            Vec<crate::ast::AssociatedTypeBinding>,
            ModuleSource,
        )> = Vec::new();

        // Check specific module if provided
        if !module_path.is_empty()
            && let Some(module) = self.loaded_modules.get(module_path)
        {
            let impl_module_source = ModuleSource::from_path(module_path);
            for item in &module.items {
                if let Item::Impl(impl_block) = item
                    && let Some(trait_type) = &impl_block.trait_type
                {
                    impl_blocks_to_check.push((
                        impl_block.ty.clone(),
                        trait_type.clone(),
                        impl_block.methods.clone(),
                        impl_block.associated_types.clone(),
                        impl_module_source.clone(),
                    ));
                }
            }
        }

        // Also check all loaded modules
        for (path, module) in self.loaded_modules {
            let impl_module_source = ModuleSource::from_path(path);
            for item in &module.items {
                if let Item::Impl(impl_block) = item
                    && let Some(trait_type) = &impl_block.trait_type
                {
                    impl_blocks_to_check.push((
                        impl_block.ty.clone(),
                        trait_type.clone(),
                        impl_block.methods.clone(),
                        impl_block.associated_types.clone(),
                        impl_module_source.clone(),
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
                    self.current_module_source.clone(),
                ));
            }
        }

        // For newtypes, also check base type for trait implementations
        let names_to_check: Vec<String> = {
            let mut names = vec![struct_name.to_string()];
            // If struct_name is a newtype, also check base type names
            if let Some(&newtype_id) = self.type_aliases.get(struct_name) {
                let mut current = newtype_id;
                loop {
                    match self.type_table.borrow().get(current).clone() {
                        ResolvedType::Newtype { base_type, .. } => {
                            let base_name = self.type_table.borrow().type_name(base_type);
                            names.push(base_name);
                            current = base_type;
                        }
                        _ => break,
                    }
                }
            }
            names
        };

        // Now process the collected impl blocks with mutable access
        for (impl_ty, trait_type, methods, associated_types, impl_module_source) in
            impl_blocks_to_check
        {
            let impl_struct_name = self.get_type_name(&impl_ty);
            if names_to_check.contains(&impl_struct_name) {
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
                        let param_types = self.extract_param_types(&method.params);
                        found_traits.push((
                            trait_name,
                            MethodInfo {
                                return_type,
                                self_kind,
                                param_types,
                                inherited_from_base: None,
                            },
                            impl_module_source.clone(),
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

    /// Find arithmetic trait implementation (`Add`, `Sub`, `Mul`, `Div`, `Rem`)
    fn find_arithmetic_trait_impl(
        &mut self,
        struct_name: &str,
        base_type_id: TypeId,
        trait_name: &str,
        method_name: &str,
    ) -> Option<ArithmeticTraitInfo> {
        // Get concrete type arguments from the base type (for generic instances)
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

            // Check if this is the target trait
            let found_trait_name = self.get_type_name(&trait_type);
            if found_trait_name != trait_name {
                continue;
            }

            // Build type parameter mapping from impl_ty to concrete types
            let type_param_mapping = Self::build_type_param_mapping(&impl_ty, &concrete_type_args);

            // Find the method
            for method in &methods {
                if method.name == method_name {
                    // Set up associated type bindings
                    let mut assoc_type_map: HashMap<String, TypeId> = HashMap::new();

                    // Process associated types (e.g., `type Output = Self`)
                    for assoc in &associated_types {
                        let resolved_type =
                            self.resolve_type_with_param_mapping(&assoc.ty, &type_param_mapping);
                        assoc_type_map.insert(assoc.name.clone(), resolved_type);
                    }

                    // Get the output type from associated types
                    let output_type = assoc_type_map
                        .get("Output")
                        .copied()
                        .unwrap_or(base_type_id);

                    let self_kind = method
                        .params
                        .first()
                        .map(|p| p.self_kind)
                        .unwrap_or(ast::SelfKind::None);

                    return Some(ArithmeticTraitInfo {
                        output_type,
                        self_kind,
                        trait_name: trait_name.to_string(),
                    });
                }
            }
        }

        None
    }

    /// Check if a type implements a specific trait (for trait bound checking)
    fn type_implements_trait(&self, type_id: TypeId, trait_name: &str) -> bool {
        let resolved = self.type_table.borrow().get(type_id).clone();

        // Primitives have built-in implementations for certain traits
        if let ResolvedType::Primitive(prim) = &resolved {
            match trait_name {
                // All primitives implement Eq and Ord
                "Eq" | "Ord" => return true,
                // Add other built-in trait implementations as needed
                _ => {}
            }
            // For other traits, check the type name
            let type_name = format!("{prim:?}").to_lowercase();
            return self.find_trait_impl_for_type(&type_name, trait_name);
        }

        // Get the type name for looking up implementations
        let type_name = match &resolved {
            ResolvedType::Struct { name, .. } => name.clone(),
            ResolvedType::GenericInstance { name, .. } => name.clone(),
            ResolvedType::Option(_) => "Option".to_string(),
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
                // For references, check if the inner type implements the trait
                return self.type_implements_trait(*inner, trait_name);
            }
            _ => return false,
        };

        self.find_trait_impl_for_type(&type_name, trait_name)
    }

    /// Helper to check if there's an impl block for a type implementing a trait
    fn find_trait_impl_for_type(&self, type_name: &str, trait_name: &str) -> bool {
        // Check all loaded modules
        for module in self.loaded_modules.values() {
            for item in &module.items {
                if let Item::Impl(impl_block) = item
                    && let Some(trait_type) = &impl_block.trait_type
                {
                    let impl_type_name = self.get_type_name(&impl_block.ty);
                    let impl_trait_name = self.get_type_name(trait_type);

                    if impl_type_name == type_name && impl_trait_name == trait_name {
                        return true;
                    }
                }
            }
        }

        // Check current module items
        for item in &self.current_module_items {
            if let Item::Impl(impl_block) = item
                && let Some(trait_type) = &impl_block.trait_type
            {
                let impl_type_name = self.get_type_name(&impl_block.ty);
                let impl_trait_name = self.get_type_name(trait_type);

                if impl_type_name == type_name && impl_trait_name == trait_name {
                    return true;
                }
            }
        }

        false
    }

    /// Convert a `TypeId` to a human-readable string for error messages
    fn type_id_to_string(&self, type_id: TypeId) -> String {
        let resolved = self.type_table.borrow().get(type_id).clone();
        match resolved {
            ResolvedType::Primitive(prim) => format!("{prim:?}").to_lowercase(),
            ResolvedType::Struct { name, .. } => name,
            ResolvedType::GenericInstance {
                name, type_args, ..
            } => {
                if type_args.is_empty() {
                    name
                } else {
                    let args: Vec<String> = type_args
                        .iter()
                        .map(|&t| self.type_id_to_string(t))
                        .collect();
                    format!("{}<{}>", name, args.join(", "))
                }
            }
            ResolvedType::Option(inner) => format!("Option<{}>", self.type_id_to_string(inner)),
            ResolvedType::BuiltinArray(elem) => {
                format!("builtin::array<{}>", self.type_id_to_string(elem))
            }
            ResolvedType::Ref(inner) => format!("&{}", self.type_id_to_string(inner)),
            ResolvedType::MutRef(inner) => format!("&mut {}", self.type_id_to_string(inner)),
            ResolvedType::Tuple(elems) => {
                let parts: Vec<String> = elems.iter().map(|&t| self.type_id_to_string(t)).collect();
                format!("[{}]", parts.join(", "))
            }
            ResolvedType::Function {
                params,
                return_type,
                ..
            } => {
                let param_strs: Vec<String> =
                    params.iter().map(|&t| self.type_id_to_string(t)).collect();
                let ret_str = self.type_id_to_string(return_type);
                format!("fn({}) -> {}", param_strs.join(", "), ret_str)
            }
            ResolvedType::TypeParam { name, .. } => name,
            ResolvedType::Unit => "()".to_string(),
            ResolvedType::Never => "!".to_string(),
            ResolvedType::Unknown => "<unknown>".to_string(),
            ResolvedType::Error => "<error>".to_string(),
            _ => format!("{resolved:?}"),
        }
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

                    // trait_name is already base name (get_type_name returns name without type args)
                    return Some((assoc_type, self_kind, trait_name));
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

                // Special-case Option and Result to use their dedicated types
                // (required for pattern matching to work correctly)
                let base_name = &g.name;
                match base_name.as_str() {
                    "Option" => {
                        let inner = resolved_args.first().copied().unwrap_or(TypeTable::UNKNOWN);
                        self.type_table.borrow_mut().make_option(inner)
                    }
                    "Result" => {
                        let ok = resolved_args.first().copied().unwrap_or(TypeTable::UNKNOWN);
                        let err = resolved_args.get(1).copied().unwrap_or(TypeTable::UNKNOWN);
                        self.type_table.borrow_mut().make_result(ok, err)
                    }
                    _ => {
                        // For other generic types, create a generic instance
                        self.type_table
                            .borrow_mut()
                            .intern(ResolvedType::GenericInstance {
                                name: base_name.clone(),
                                module_source: self.current_module_source.clone(),
                                type_args: resolved_args,
                            })
                    }
                }
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
                ..
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
                    ..
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
                _ => (
                    self.type_table
                        .borrow()
                        .mangle_type_name(output_base_type_id),
                    vec![],
                    None,
                ),
            };

        // Look up method info to check if it needs &mut self
        let mut method_info = self.lookup_method_info(output_type, &method_call.method);
        let mut method_trait_name: Option<String> = None;
        let mut method_trait_impl_source: Option<ModuleSource> = None;

        if method_info.is_none()
            && let Some((found_trait, info, impl_source)) = self.find_trait_method_for_type(
                &output_struct_name,
                &method_call.method,
                &output_module_path,
                output_type_args.as_deref(),
            )
        {
            method_trait_name = Some(found_trait);
            method_info = Some(info);
            method_trait_impl_source = Some(impl_source);
        }

        let MethodInfo {
            return_type,
            self_kind,
            param_types: _,
            inherited_from_base: _,
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

        // Use trait impl module source if this is a trait method, otherwise current module
        let method_call_module_source =
            method_trait_impl_source.unwrap_or_else(|| self.current_module_source.clone());

        Some(TirExpr::new(
            TirExprKind::MethodCall {
                receiver: Box::new(receiver_for_method),
                func: FunctionRef::External {
                    module_source: method_call_module_source,
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
            // Newtype - look through to base type for field access
            ResolvedType::Newtype { base_type, .. } => {
                return self.lookup_field_type(base_type, field_name, _span);
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
                value: ast::Literal::Number(num_lit),
                ..
            }) = &index.index
                && !Self::is_float_only_literal(&num_lit.repr)
                && let Ok(idx) = num_lit.repr.parse::<usize>()
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
                // If statement can produce a value if both branches exist and have the same type
                TirStmtKind::If {
                    then_block,
                    else_block: Some(else_block),
                    ..
                } => {
                    let then_type = Self::block_result_type(then_block);
                    let else_type = Self::block_result_type(else_block);
                    if then_type == else_type {
                        Some(then_type)
                    } else {
                        None
                    }
                }
                // IfPattern can also produce a value if both branches exist and have the same type
                TirStmtKind::IfPattern {
                    then_block,
                    else_block: Some(else_block),
                    ..
                } => {
                    let then_type = Self::block_result_type(then_block);
                    let else_type = Self::block_result_type(else_block);
                    if then_type == else_type {
                        Some(then_type)
                    } else {
                        None
                    }
                }
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
            ast::Condition::Expr(expr) => self.resolve_expr(expr, ctx),
            ast::Condition::Pattern { span, .. } => {
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
        let scrutinee = self.resolve_expr(&match_expr.expr, ctx);
        let scrutinee_type = scrutinee.type_id;

        let arms: Vec<TirMatchArm> = match_expr
            .arms
            .iter()
            .map(|arm| self.resolve_match_arm(arm, scrutinee_type, ctx))
            .collect();

        // Match expression type is the type of the first arm body
        let type_id = arms
            .first()
            .map(|a| a.body.type_id)
            .unwrap_or(TypeTable::UNIT);

        TirExpr::new(
            TirExprKind::Match {
                expr: Box::new(scrutinee),
                arms,
            },
            type_id,
            match_expr.span,
        )
    }

    /// Resolve a match arm with scrutinee type information
    fn resolve_match_arm(
        &mut self,
        arm: &MatchArm,
        scrutinee_type: TypeId,
        ctx: &mut FunctionContext,
    ) -> TirMatchArm {
        // Enter scope for pattern bindings (they're only visible in the arm body)
        ctx.enter_scope();

        // Resolve pattern with scrutinee type information (same as if let)
        let pattern = self.resolve_if_pattern(&arm.pattern, scrutinee_type, ctx, arm.span);

        // Resolve arm body
        let body = self.resolve_expr(&arm.body, ctx);

        ctx.exit_scope();

        TirMatchArm {
            pattern,
            body,
            span: arm.span,
        }
    }

    /// Resolve a match expression with an expected result type for arm coercion
    fn resolve_match_expr_with_expected_type(
        &mut self,
        match_expr: &ast::MatchExpr,
        ctx: &mut FunctionContext,
        expected_type: TypeId,
    ) -> TirExpr {
        let scrutinee = self.resolve_expr(&match_expr.expr, ctx);
        let scrutinee_type = scrutinee.type_id;

        let arms: Vec<TirMatchArm> = match_expr
            .arms
            .iter()
            .map(|arm| {
                self.resolve_match_arm_with_expected_type(arm, scrutinee_type, ctx, expected_type)
            })
            .collect();

        TirExpr::new(
            TirExprKind::Match {
                expr: Box::new(scrutinee),
                arms,
            },
            expected_type,
            match_expr.span,
        )
    }

    /// Resolve a match arm with expected body type for coercion
    fn resolve_match_arm_with_expected_type(
        &mut self,
        arm: &MatchArm,
        scrutinee_type: TypeId,
        ctx: &mut FunctionContext,
        expected_type: TypeId,
    ) -> TirMatchArm {
        ctx.enter_scope();

        let pattern = self.resolve_if_pattern(&arm.pattern, scrutinee_type, ctx, arm.span);

        // Resolve arm body with expected type for coercion
        let body = self.resolve_expr_with_expected_type(&arm.body, ctx, Some(expected_type));

        ctx.exit_scope();

        TirMatchArm {
            pattern,
            body,
            span: arm.span,
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
                functor_id: None, // Assigned during lowering
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
                        // Call to_string method - determine correct module source based on type
                        // Follow newtypes and references to find the ultimate base type
                        let base_type_id = self.get_ultimate_base_type(resolved.type_id);
                        let (receiver_type_name, method_module_source) =
                            match self.type_table.borrow().get(base_type_id).clone() {
                                ResolvedType::Struct {
                                    name,
                                    module_source,
                                    ..
                                } => (name.clone(), module_source),
                                ResolvedType::GenericInstance {
                                    name,
                                    module_source,
                                    ..
                                } => (name.clone(), module_source),
                                ResolvedType::Primitive(_) => {
                                    // Primitive to_string methods are in core:prelude/primitives
                                    let prim_module = ModuleSource::Core {
                                        name: "prelude/primitives".to_string(),
                                    };
                                    (
                                        self.type_table.borrow().mangle_type_name(base_type_id),
                                        prim_module,
                                    )
                                }
                                _ => {
                                    // Fallback to current module
                                    (
                                        self.type_table.borrow().mangle_type_name(resolved.type_id),
                                        self.current_module_source.clone(),
                                    )
                                }
                            };
                        let mangled_method_name = format!("{receiver_type_name}::to_string");
                        TirExpr::new(
                            TirExprKind::MethodCall {
                                receiver: Box::new(resolved.clone()),
                                func: FunctionRef::External {
                                    module_source: method_module_source,
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

        // Build a chain of pairwise string concatenations: String::concat(String::concat(a, b), c)
        // String::concat only takes 2 arguments, so we chain them
        let mut result = parts.remove(0);
        for part in parts {
            result = TirExpr::new(
                TirExprKind::StaticCall {
                    func: FunctionRef::External {
                        module_source: string_module_source(),
                        name: "String::concat".to_string(),
                        monomorph_info: None,
                        method_info: Some(LocalMethodName::new(
                            "String".to_string(),
                            None, // Static methods are inherent, no trait
                            "concat".to_string(),
                        )),
                    },
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

        // Cast to i128/u128: expr as u128 → u128::from_u64(expr as u64)
        // For large literals: 170... as i128 → i128::from_string("170...")
        let struct_name = match self.type_table.borrow().get(target_type).clone() {
            ResolvedType::Struct { name, .. } => Some(name),
            _ => None,
        };

        if let Some(ref name) = struct_name
            && (name == "u128" || name == "i128")
        {
            // Handle number literal cast specially to support values > u64
            if let ast::Expr::Literal(lit) = &cast.expr
                && let Literal::Number(num_lit) = &lit.value
                && !Self::is_float_only_literal(&num_lit.repr)
            {
                // Try to parse as u64
                if let Ok(value) = Self::parse_int_literal(&num_lit.repr) {
                    // For u128: value fits in u64, use from_u64
                    // For i128: value must also fit in i64 (positive range), otherwise use from_string
                    let use_from_u64_or_i64 = if name == "u128" {
                        true // u64 always works for u128
                    } else {
                        // For i128, check if value fits in i64 positive range
                        i64::try_from(value).is_ok()
                    };

                    if use_from_u64_or_i64 {
                        let (inner_type, method_name) = if name == "u128" {
                            (TypeTable::U64, "from_u64")
                        } else {
                            (TypeTable::I64, "from_i64")
                        };

                        let inner_literal = TirExpr::new(
                            TirExprKind::IntLiteral {
                                value,
                                repr: num_lit.repr.clone(),
                            },
                            inner_type,
                            lit.span,
                        );

                        let mangled_func_name = format!("{name}::{method_name}");
                        let method_info =
                            LocalMethodName::new(name.clone(), None, method_name.to_string());

                        return TirExpr::new(
                            TirExprKind::StaticCall {
                                func: FunctionRef::External {
                                    module_source: ModuleSource::core("prelude/int128"),
                                    name: mangled_func_name,
                                    monomorph_info: None,
                                    method_info: Some(method_info),
                                },
                                args: vec![inner_literal],
                            },
                            target_type,
                            cast.span,
                        );
                    }
                }

                // Value doesn't fit in u64 (or i64 for i128), use from_pair
                if name == "u128" {
                    if let Ok(value) = Self::parse_u128_literal(&num_lit.repr) {
                        let (low, high) = Self::unpack_u128(value);
                        return self.build_from_pair_call(
                            name,
                            low,
                            high as i64,
                            target_type,
                            cast.span,
                        );
                    }
                    self.errors.push(TypeError::InvalidLiteral {
                        message: format!("invalid u128 literal: {}", num_lit.repr),
                        span: lit.span,
                    });
                } else {
                    // i128
                    if let Ok(value) = Self::parse_i128_literal(&num_lit.repr) {
                        let (low, high) = Self::unpack_i128(value);
                        return self.build_from_pair_call(name, low, high, target_type, cast.span);
                    }
                    self.errors.push(TypeError::InvalidLiteral {
                        message: format!("invalid i128 literal: {}", num_lit.repr),
                        span: lit.span,
                    });
                }
            }

            // Handle negated number literal cast: -170... as i128
            if let ast::Expr::Unary(unary) = &cast.expr
                && unary.op == ast::UnaryOp::Neg
                && let ast::Expr::Literal(lit) = &unary.expr
                && let Literal::Number(num_lit) = &lit.value
                && !Self::is_float_only_literal(&num_lit.repr)
                && name == "i128"
            {
                // Parse the negated value directly using Rust's i128
                let negated_repr = format!("-{}", Self::clean_literal_repr(&num_lit.repr));
                if let Ok(value) = Self::parse_i128_literal(&negated_repr) {
                    let (low, high) = Self::unpack_i128(value);
                    return self.build_from_pair_call(name, low, high, target_type, unary.span);
                }
                self.errors.push(TypeError::InvalidLiteral {
                    message: format!("invalid i128 literal: -{}", num_lit.repr),
                    span: unary.span,
                });
            }

            // General expression cast (not a literal)
            let expr_resolved = self.resolve_expr(&cast.expr, ctx);
            let source_type = expr_resolved.type_id;

            // Check if source type is a numeric type we can convert from
            if self.type_table.borrow().is_integer(source_type)
                || self.type_table.borrow().is_float(source_type)
            {
                // Determine intermediate type and method based on target
                let (intermediate_type, method_name) = if name == "u128" {
                    (TypeTable::U64, "from_u64")
                } else {
                    (TypeTable::I64, "from_i64")
                };

                // Cast the expression to intermediate type first
                let casted_expr = TirExpr::new(
                    TirExprKind::Cast {
                        expr: Box::new(expr_resolved),
                        target_type: intermediate_type,
                    },
                    intermediate_type,
                    cast.span,
                );

                // Build static call: u128::from_u64(expr as u64)
                // Note: i128/u128 are defined in core:prelude/int128
                let mangled_func_name = format!("{name}::{method_name}");
                let method_info = LocalMethodName::new(name.clone(), None, method_name.to_string());

                return TirExpr::new(
                    TirExprKind::StaticCall {
                        func: FunctionRef::External {
                            module_source: ModuleSource::core("prelude/int128"),
                            name: mangled_func_name,
                            monomorph_info: None,
                            method_info: Some(method_info),
                        },
                        args: vec![casted_expr],
                    },
                    target_type,
                    cast.span,
                );
            }
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
            TirStmtKind::Loop { body } | TirStmtKind::LabeledBlock { block: body, .. } => {
                Self::find_return_type_in_block(body)
            }
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
                // Find expected field type for literal coercion (only for numeric literals)
                // We only use expected type for numeric literals (including negated ones)
                // to avoid interfering with tuple-to-array coercion for generic struct fields
                let is_numeric_literal = matches!(
                    &field.value,
                    ast::Expr::Literal(lit) if matches!(
                        &lit.value,
                        ast::Literal::Number(_)
                    )
                ) || matches!(
                    &field.value,
                    ast::Expr::Unary(unary) if unary.op == ast::UnaryOp::Neg && matches!(
                        &unary.expr,
                        ast::Expr::Literal(lit) if matches!(
                            &lit.value,
                            ast::Literal::Number(_)
                        )
                    )
                );

                let expected_field_type = if is_numeric_literal {
                    struct_field_types
                        .iter()
                        .find(|(name, _)| name == &field.name)
                        .map(|(_, type_id)| *type_id)
                } else {
                    None
                };

                // Use expected type for literal coercion (e.g., 0 -> u64 when field is u64)
                let mut value =
                    self.resolve_expr_with_expected_type(&field.value, ctx, expected_field_type);

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
        let (struct_type, mangled_struct_name, fields) =
            if self.generic_struct_names.contains(&struct_name) {
                // This is a generic struct - infer type arguments from field values
                let type_args = self.infer_type_args_from_fields(&struct_name, &fields);

                // Substitute type parameters in field value types.
                // This is necessary for empty array literals in self-referential fields
                // (e.g., `children: []` in `Node<K> { children: Array<&Node<K>> }`)
                // which get typed with TypeParams before inference.
                let fields: Vec<TirStructField> = if type_args.is_empty() {
                    fields
                } else {
                    fields
                        .into_iter()
                        .map(|mut field| {
                            field.value.type_id =
                                self.substitute_type_params(field.value.type_id, &type_args);
                            field
                        })
                        .collect()
                };

                let struct_type = self.type_table.borrow_mut().make_generic_instance(
                    struct_name.clone(),
                    ModuleSource::from_path(&module_path),
                    type_args.clone(),
                );
                // Build mangled name with type arguments
                let arg_names: Vec<String> = type_args
                    .iter()
                    .map(|&t| self.type_table.borrow().type_name(t))
                    .collect();
                let mangled_name = mangle_generic_name(&struct_name, &arg_names);
                (struct_type, mangled_name, fields)
            } else {
                let struct_type = self
                    .type_table
                    .borrow_mut()
                    .make_struct(struct_name.clone(), ModuleSource::from_path(&module_path));
                (struct_type, struct_name, fields)
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
                // Only insert if we don't already have a concrete mapping for this type param.
                // This prevents later fields with self-referential types (like Array<&Node<K>>)
                // from overwriting earlier correct mappings (like K -> String) with
                // incorrect mappings (like K -> K).
                type_param_map.entry(expected).or_insert(actual);
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
            "u8" => TypeTable::U8,
            "u16" => TypeTable::U16,
            "u32" => TypeTable::U32,
            "u64" => TypeTable::U64,
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
                } else if let Some(enum_info) = self.enum_cases.get(name) {
                    // It's an enum - use the module source where it was defined
                    self.type_table
                        .borrow_mut()
                        .make_enum(name.to_string(), enum_info.module_source.clone())
                } else if let Some(resource_info) = self.resource_types.get(name) {
                    // It's a resource - use the module source where it was defined
                    self.type_table
                        .borrow_mut()
                        .make_resource(name.to_string(), resource_info.module_source.clone())
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

                    // Get struct info for module source and bounds checking
                    let struct_info = self.struct_fields.get(name).cloned();
                    let module_source = struct_info
                        .as_ref()
                        .map(|info| info.module_source.clone())
                        .unwrap_or_else(|| self.current_module_source.clone());

                    // Check trait bounds for each type argument
                    if let Some(info) = &struct_info {
                        for (i, (param_name, bounds)) in info.type_param_bounds.iter().enumerate() {
                            if let Some(&type_arg) = type_args.get(i) {
                                for bound in bounds {
                                    if !self.type_implements_trait(type_arg, bound) {
                                        // Get the type name for the error message
                                        let type_name = self.type_id_to_string(type_arg);
                                        self.errors.push(TypeError::TraitBoundNotSatisfied {
                                            type_name,
                                            trait_name: bound.clone(),
                                            param_name: param_name.clone(),
                                            span,
                                        });
                                    }
                                }
                            }
                        }
                    }

                    // Create a GenericInstance type
                    self.type_table.borrow_mut().make_generic_instance(
                        name.to_string(),
                        module_source,
                        type_args,
                    )
                } else if let Some(variant_info) = self.variant_cases.get(name).cloned() {
                    // Check if it's a generic variant (like Result<T, E>)
                    if variant_info.type_params.is_empty() {
                        TypeTable::UNKNOWN
                    } else {
                        let type_args: Vec<TypeId> =
                            args.iter().map(|t| self.resolve_type(t)).collect();
                        self.type_table.borrow_mut().make_generic_instance(
                            name.to_string(),
                            variant_info.module_source,
                            type_args,
                        )
                    }
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

    // Build registries once here, shared across all subsequent phases
    let (wasi_registry, world_registry) = crate::component_model::WasiRegistry::build_from_stdlib();

    // Build builtin registry (uses a temporary type table for type resolution)
    let temp_type_table = std::cell::RefCell::new(crate::tir::TypeTable::new());
    let builtin_registry =
        crate::builtin_registry::BuiltinRegistry::build_from_stdlib(&temp_type_table);

    Ok(Project::new(
        entry_module_source,
        tir_modules_by_source,
        symbols,
        implicit_modules_by_source,
        module_name,
        wasi_registry,
        world_registry,
        builtin_registry,
    ))
}
