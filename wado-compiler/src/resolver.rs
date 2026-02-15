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
use std::collections::VecDeque;
use std::rc::Rc;

use indexmap::{IndexMap, IndexSet};

use crate::builtin_registry::BuiltinRegistry;
use crate::component_model::WasiRegistry;
use crate::name::{self as name, LocalMethodName, MethodName, ModuleSource, mangle_generic_name};

use crate::ast::{
    self, BinaryOp, Block, BreakStmt, ContinueStmt, Expr, ExprStmt, Function, GlobalDecl, IfExpr,
    IfStmt, Item, LetStmt, Literal, LoopStmt, MatchArm, Module, Pattern, ReturnStmt, Stmt, Type,
    UnaryOp,
};
use crate::compiler_host::CompilerHost;
use crate::logger::{Bail, Logger};
use crate::project::Project;
use crate::symbol::{SymbolKind, SymbolTable};
use crate::tir::{
    FunctionRef, MonomorphInfo, PrimitiveType, ResolvedType, SubstitutionContext, TirBinaryOp,
    TirBlock, TirCapture, TirEnum, TirEnumCase, TirExpr, TirExprKind, TirFunction, TirGlobal,
    TirLiteralPattern, TirMatchArm, TirModule, TirParam, TirPattern, TirStmt, TirStmtKind,
    TirStruct, TirStructField, TirTest, TirUnaryOp, TirVariantCase, TirVariantDecl, TypeId,
    TypeTable,
};
use crate::token::Span;

/// Helper to get the `ModuleSource` for String type (core:prelude/string.wado)
fn string_module_source() -> ModuleSource {
    ModuleSource::core("prelude/string.wado")
}

/// Parsed format specification from a template string interpolation.
/// Syntax: `[[fill]align][sign][#][0][width][.precision]type`
#[allow(dead_code)]
struct ParsedFormatSpec {
    fill: Option<char>,
    align: Option<char>, // '<', '^', '>'
    sign_plus: bool,
    alternate: bool,
    zero_pad: bool,
    width: Option<i64>,
    precision: Option<i64>,
    type_char: Option<char>, // 'b','o','x','X','e','E','?'
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

    /// Invalid type cast
    InvalidCast {
        from: String,
        to: String,
        hint: String,
        span: Span,
    },
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
            TypeError::InvalidCast {
                from,
                to,
                hint,
                span,
            } => {
                write!(
                    f,
                    "{}:{}: cannot cast '{}' to '{}': {}",
                    span.line, span.column, from, to, hint
                )
            }
        }
    }
}

impl std::error::Error for TypeError {}

impl From<TypeError> for crate::compiler_host::Diagnostic {
    fn from(e: TypeError) -> Self {
        use crate::compiler_host::{Code, DiagnosticSpan, Severity};
        let (code, message, span) = match &e {
            TypeError::TypeMismatch {
                expected,
                found,
                span,
            } => (
                Code::TypeMismatch,
                format!("type mismatch: expected '{expected}', found '{found}'"),
                *span,
            ),
            TypeError::UnknownType { name, span } => {
                (Code::UnknownType, format!("unknown type '{name}'"), *span)
            }
            TypeError::UnknownFunction { name, span } => (
                Code::UndefinedVariable,
                format!("unknown function '{name}'"),
                *span,
            ),
            TypeError::UnknownIdentifier { name, span } => (
                Code::UndefinedVariable,
                format!("unknown identifier '{name}'"),
                *span,
            ),
            TypeError::FieldNotFound {
                struct_name,
                field_name,
                span,
            } => (
                Code::TypeMismatch,
                format!("field '{field_name}' not found on struct '{struct_name}'"),
                *span,
            ),
            TypeError::ArgumentCountMismatch {
                expected,
                found,
                span,
            } => (
                Code::TypeMismatch,
                format!("expected {expected} arguments, found {found}"),
                *span,
            ),
            TypeError::InvalidLiteral { message, span } => {
                (Code::InvalidSyntax, message.clone(), *span)
            }
            TypeError::NotYetImplemented { feature, span } => (
                Code::UnsupportedFeature,
                format!("{feature} is not yet implemented"),
                *span,
            ),
            TypeError::CannotAssign { message, span } => (
                Code::ImmutableAssignment,
                format!("cannot assign: {message}"),
                *span,
            ),
            TypeError::TraitBoundNotSatisfied {
                type_name,
                trait_name,
                param_name,
                span,
            } => (
                Code::TypeMismatch,
                format!(
                    "type '{type_name}' does not implement trait '{trait_name}' required by bound on '{param_name}'"
                ),
                *span,
            ),
            TypeError::InvalidPattern { message, span } => (
                Code::InvalidSyntax,
                format!("invalid pattern: {message}"),
                *span,
            ),
            TypeError::InvalidCast {
                from,
                to,
                hint,
                span,
            } => (
                Code::InvalidCast,
                format!("cannot cast '{from}' to '{to}': {hint}"),
                *span,
            ),
        };
        crate::compiler_host::Diagnostic {
            severity: Severity::Error,
            code,
            message,
            span: Some(DiagnosticSpan::from_span(&span, None)),
        }
    }
}

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
    scopes: Vec<IndexMap<String, LocalVar>>,
    /// Next local index (Wasm locals are function-wide)
    next_local: u32,
    /// Return type of the function
    #[allow(dead_code)] // For future return type checking
    return_type: TypeId,
    /// Local variable types in order (for Wasm local declarations)
    local_types: Vec<TypeId>,
    /// Local indices that have their address taken (&x or &mut x)
    address_taken_locals: IndexSet<u32>,
    /// Outer context locals for closure capture detection (name -> `LocalVar` snapshot)
    /// Only set for closure contexts
    outer_locals: IndexMap<String, LocalVar>,
    /// Captured variables detected during resolution (name -> capture index)
    /// Only used for closure contexts
    captured_vars: IndexMap<String, u32>,
    /// Stack of labeled block expression targets for tracking break types
    labeled_block_targets: Vec<LabeledBlockTarget>,
    /// Stack of all active labels (from labeled blocks and labeled block expressions)
    active_labels: Vec<String>,
    /// Current function name for `#function` compile-time literal
    function_name: String,
}

impl FunctionContext {
    fn new(return_type: TypeId, function_name: String) -> Self {
        Self {
            scopes: vec![IndexMap::new()], // Start with one scope for function parameters
            next_local: 0,
            return_type,
            local_types: Vec::new(),
            address_taken_locals: IndexSet::new(),
            outer_locals: IndexMap::new(),
            captured_vars: IndexMap::new(),
            labeled_block_targets: Vec::new(),
            active_labels: Vec::new(),
            function_name,
        }
    }

    /// Create a closure context with outer scope access for capture detection
    fn new_closure(return_type: TypeId, outer_ctx: &FunctionContext) -> Self {
        // Snapshot all locals from outer context
        let mut outer_locals = IndexMap::new();
        for scope in &outer_ctx.scopes {
            for (name, local) in scope {
                outer_locals.insert(name.clone(), local.clone());
            }
        }

        // Closure function name is parent::{closure}
        let function_name = format!("{}::{{closure}}", outer_ctx.function_name);

        Self {
            scopes: vec![IndexMap::new()],
            next_local: 0,
            return_type,
            local_types: Vec::new(),
            address_taken_locals: IndexSet::new(),
            outer_locals,
            captured_vars: IndexMap::new(),
            labeled_block_targets: Vec::new(),
            active_labels: Vec::new(),
            function_name,
        }
    }

    /// Enter a new scope (for blocks, if/while/for/loop bodies)
    fn enter_scope(&mut self) {
        self.scopes.push(IndexMap::new());
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
pub struct Resolver<'a, H: CompilerHost> {
    /// Type table (shared across all modules via Rc<RefCell>)
    type_table: Rc<RefCell<TypeTable>>,
    /// Symbol table from analyzer
    #[allow(dead_code)]
    symbols: &'a SymbolTable,
    /// Loaded modules from analyzer
    #[allow(dead_code)]
    loaded_modules: &'a IndexMap<ModuleSource, Module>,
    /// Newtypes (name -> resolved type) - flat map for current module
    newtypes: IndexMap<String, TypeId>,
    /// Struct field info (struct name -> (`module_source`, fields)) - flat map for current module
    struct_fields: IndexMap<String, StructFieldInfo>,
    /// Variant case info (variant name -> (`module_source`, `type_params`, cases)) - flat map for current module
    variant_cases: IndexMap<String, VariantInfo>,
    /// Enum case info (enum name -> (`module_source`, cases)) - flat map for current module
    enum_cases: IndexMap<String, EnumInfo>,
    /// Resource info (resource name -> module source and methods) - flat map for current module
    resource_types: IndexMap<String, ResourceInfo>,
    /// Per-module nested maps for cross-module type resolution
    all_newtypes: IndexMap<ModuleSource, IndexMap<String, TypeId>>,
    all_struct_fields: IndexMap<ModuleSource, IndexMap<String, StructFieldInfo>>,
    all_variant_cases: IndexMap<ModuleSource, IndexMap<String, VariantInfo>>,
    all_enum_cases: IndexMap<ModuleSource, IndexMap<String, EnumInfo>>,
    all_resource_types: IndexMap<ModuleSource, IndexMap<String, ResourceInfo>>,
    /// Function return types (name -> return type)
    function_return_types: IndexMap<String, TypeId>,
    /// Imported function names for the current module
    imported_functions: IndexSet<String>,
    /// Logger for emitting diagnostics
    logger: &'a Logger<'a, H>,
    /// Current module source being resolved (for struct type `module_source`)
    current_module_source: ModuleSource,
    /// Current module items (for local function parameter lookup)
    current_module_items: Vec<Item>,
    /// Type parameters currently in scope (name -> (index, `TypeId`))
    /// Set when resolving generic structs or functions
    current_type_params: IndexMap<String, (u32, TypeId)>,
    /// Trait bounds on type parameters in scope (name -> trait names)
    /// Used for resolving trait methods on type params (e.g., `T.cmp()` when T: Ord)
    current_type_param_bounds: IndexMap<String, Vec<String>>,
    /// Generic struct definitions (name -> type param count)
    /// Used to determine if a struct is generic
    generic_struct_names: IndexSet<String>,
    /// Generic function type parameters (`func_name` -> `type_params`)
    /// Used for substituting type parameters in return types
    generic_function_params: IndexMap<String, Vec<(String, TypeId)>>,
    /// Generic method type parameters (`mangled_name` -> `type_params`)
    /// Used for substituting type parameters in method return types
    generic_method_params: IndexMap<String, Vec<(String, TypeId)>>,
    /// Current associated type bindings in scope (`Self::Name` -> resolved type)
    /// Set when resolving trait implementations
    current_associated_type_bindings: IndexMap<String, TypeId>,
    /// Current `Self` type in scope (the type being implemented in an impl block)
    current_self_type: Option<TypeId>,
    /// WASI registry for looking up effect return types
    wasi_registry: &'static WasiRegistry,
    /// Builtin registry for looking up builtin function return types
    builtin_registry: &'a BuiltinRegistry,
    /// Global variables in the current module (name -> (type, `is_mutable`))
    current_module_globals: IndexMap<String, (TypeId, bool)>,
    /// Imported globals (local name -> (source module, original name, type, `is_mutable`))
    imported_globals: IndexMap<String, (ModuleSource, String, TypeId, bool)>,
    /// Associated constants from impl blocks ("`TypeName::CONST`" -> (type, expr))
    /// These are inlined at every use site during resolution.
    associated_constants: IndexMap<String, (ast::Type, ast::Expr)>,
    /// Cache of per-module type maps for cross-module type resolution.
    /// Built lazily on first access per module. Avoids rebuilding `build_module_map`
    /// on every imported method call or field access.
    module_type_maps_cache: IndexMap<ModuleSource, ModuleTypeMaps>,
}

/// Cached per-module type maps for cross-module type resolution.
/// These are the five flat maps that `build_module_map` produces.
struct ModuleTypeMaps {
    struct_fields: IndexMap<String, StructFieldInfo>,
    variant_cases: IndexMap<String, VariantInfo>,
    enum_cases: IndexMap<String, EnumInfo>,
    newtypes: IndexMap<String, TypeId>,
    resource_types: IndexMap<String, ResourceInfo>,
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

impl<'a, H: CompilerHost> Resolver<'a, H> {
    /// Create a new resolver
    pub fn new(
        symbols: &'a SymbolTable,
        loaded_modules: &'a IndexMap<ModuleSource, Module>,
        builtin_registry: &'a BuiltinRegistry,
        logger: &'a Logger<'a, H>,
    ) -> Self {
        let (wasi_registry, _) = WasiRegistry::build_from_stdlib();
        let type_table = Rc::new(RefCell::new(TypeTable::new()));
        Self {
            type_table,
            symbols,
            loaded_modules,
            newtypes: IndexMap::new(),
            struct_fields: IndexMap::new(),
            variant_cases: IndexMap::new(),
            enum_cases: IndexMap::new(),
            resource_types: IndexMap::new(),
            all_newtypes: IndexMap::new(),
            all_struct_fields: IndexMap::new(),
            all_variant_cases: IndexMap::new(),
            all_enum_cases: IndexMap::new(),
            all_resource_types: IndexMap::new(),
            function_return_types: IndexMap::new(),
            imported_functions: IndexSet::new(),
            logger,
            current_module_source: ModuleSource::entry_point_with_filename("<uninitialized>"),
            current_module_items: Vec::new(),
            current_type_params: IndexMap::new(),
            current_type_param_bounds: IndexMap::new(),
            generic_struct_names: IndexSet::new(),
            generic_function_params: IndexMap::new(),
            generic_method_params: IndexMap::new(),
            current_associated_type_bindings: IndexMap::new(),
            current_self_type: None,
            wasi_registry,
            builtin_registry,
            current_module_globals: IndexMap::new(),
            imported_globals: IndexMap::new(),
            associated_constants: IndexMap::new(),
            module_type_maps_cache: IndexMap::new(),
        }
    }

    /// Resolve a module, converting AST to TIR
    pub fn resolve_module(
        &mut self,
        module: &Module,
        module_source: ModuleSource,
    ) -> Result<TirModule, Bail> {
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
                let source_module_source = name::resolve_import(&module_source, &use_decl.source);

                // Look up the source module to find global declarations
                if let Some(source_module) = self.loaded_modules.get(&source_module_source) {
                    for use_item in &use_decl.items {
                        if let ast::UseItem::Simple { name, alias } = use_item {
                            // Check if this import refers to a global variable
                            if let Some(symbol) =
                                self.symbols.lookup_in_module(&source_module_source, name)
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

        // Collect associated constants from loaded modules and current module
        self.associated_constants.clear();
        for module_items in self
            .loaded_modules
            .values()
            .map(|m| &m.items)
            .chain(std::iter::once(&module.items))
        {
            for item in module_items {
                if let Item::Impl(impl_block) = item {
                    let type_name = self.get_type_name(&impl_block.ty);
                    for assoc_const in &impl_block.constants {
                        let key = MethodName::format_local(&type_name, None, &assoc_const.name);
                        self.associated_constants
                            .insert(key, (assoc_const.ty.clone(), assoc_const.value.clone()));
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
                    let old_type_param_bounds = std::mem::take(&mut self.current_type_param_bounds);

                    // First, collect explicit type params from impl<T: Bound>
                    for param in &impl_block.type_params {
                        if !param.bounds.is_empty() {
                            self.current_type_param_bounds
                                .insert(param.name.clone(), param.bounds.clone());
                        }
                    }

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
                        // Also collect bounds from generic type args
                        // e.g., impl Array<T: Ord> has bounds on the type arg
                        for param in &impl_block.type_params {
                            if !param.bounds.is_empty()
                                && !self.current_type_param_bounds.contains_key(&param.name)
                            {
                                self.current_type_param_bounds
                                    .insert(param.name.clone(), param.bounds.clone());
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

                    // Collect explicitly provided method names
                    let provided_method_names: Vec<String> =
                        impl_block.methods.iter().map(|m| m.name.clone()).collect();

                    for method in &impl_block.methods {
                        if let Some(mut tir_func) = self.resolve_method(
                            method,
                            &struct_name,
                            &impl_block.ty,
                            trait_name.as_deref(),
                        ) {
                            tir_func.name = MethodName::format_local(
                                &struct_name,
                                trait_name.as_deref(),
                                &method.name,
                            );
                            tir_module.add_function(tir_func);
                        }
                    }

                    // For trait impls, synthesize TIR functions for default methods
                    // not explicitly provided in the impl block
                    if let Some(ref trait_n) = trait_name {
                        let default_methods: Vec<ast::Function> = self
                            .find_trait_decl_methods(trait_n)
                            .unwrap_or_default()
                            .into_iter()
                            .filter(|m| {
                                m.body.is_some() && !provided_method_names.contains(&m.name)
                            })
                            .collect();

                        for default_method in &default_methods {
                            if let Some(mut tir_func) = self.resolve_method(
                                default_method,
                                &struct_name,
                                &impl_block.ty,
                                Some(trait_n),
                            ) {
                                tir_func.name = MethodName::format_local(
                                    &struct_name,
                                    Some(trait_n),
                                    &default_method.name,
                                );
                                // Default methods from trait declarations are not marked pub
                                // in the AST, but they should be treated as pub since they are
                                // part of a trait implementation
                                tir_func.is_pub = true;
                                tir_module.add_function(tir_func);
                            }
                        }
                    }

                    // Restore old associated type bindings, type params, and bounds
                    self.current_associated_type_bindings = old_associated_type_bindings;
                    self.current_type_params = old_type_params;
                    self.current_type_param_bounds = old_type_param_bounds;
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
                Item::Enum(enum_decl) => {
                    let tir_enum = TirEnum {
                        name: enum_decl.name.clone(),
                        is_pub: enum_decl.is_pub,
                        type_params: Vec::new(),
                        monomorph_info: None,
                        cases: enum_decl
                            .cases
                            .iter()
                            .enumerate()
                            .map(|(i, case)| TirEnumCase {
                                name: case.name.clone(),
                                index: i as u32,
                                span: case.span,
                            })
                            .collect(),
                        span: enum_decl.span,
                    };
                    tir_module.add_enum(tir_enum);
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

        self.logger.ok_or_bail(tir_module)
    }

    /// Resolve all modules to TIR
    ///
    /// This resolves every module (entry and dependencies) to TIR,
    /// enabling TIR-only code generation.
    /// Modules are returned in topological order based on struct field dependencies.
    pub fn resolve_all_modules(
        symbols: &'a SymbolTable,
        modules: &'a IndexMap<ModuleSource, Module>,
        _entry_module_source: ModuleSource,
        logger: &'a Logger<'a, H>,
    ) -> Result<IndexMap<ModuleSource, TirModule>, Bail> {
        let mut result = IndexMap::new();

        // Create a shared type table wrapped in Rc<RefCell<>> for cross-module sharing
        let type_table = Rc::new(RefCell::new(TypeTable::new()));
        let mut all_newtypes: IndexMap<ModuleSource, IndexMap<String, TypeId>> = IndexMap::new();
        let mut all_struct_fields: IndexMap<ModuleSource, IndexMap<String, StructFieldInfo>> =
            IndexMap::new();
        let mut all_variant_cases: IndexMap<ModuleSource, IndexMap<String, VariantInfo>> =
            IndexMap::new();
        let mut all_enum_cases: IndexMap<ModuleSource, IndexMap<String, EnumInfo>> =
            IndexMap::new();
        let mut all_resource_types: IndexMap<ModuleSource, IndexMap<String, ResourceInfo>> =
            IndexMap::new();

        // First pass: collect struct, variant, enum, and resource names from all modules (for forward references)
        for (module_source, module) in modules {
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
                        all_struct_fields
                            .entry(module_source.clone())
                            .or_default()
                            .insert(
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
                        all_variant_cases
                            .entry(module_source.clone())
                            .or_default()
                            .insert(
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
                        all_enum_cases
                            .entry(module_source.clone())
                            .or_default()
                            .insert(
                                enum_decl.name.clone(),
                                EnumInfo {
                                    module_source: module_source.clone(),
                                    cases: Vec::new(),
                                },
                            );
                    }
                    Item::Resource(resource_decl) => {
                        all_resource_types
                            .entry(module_source.clone())
                            .or_default()
                            .insert(
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

        // Second sub-pass: resolve struct fields and newtypes
        for (module_source, module) in modules {
            // Build imported type sources for this module
            let (imported_type_sources, import_original_names) =
                Self::build_imported_type_sources(module, module_source);

            // Build module-specific flat maps for resolving types in this module
            let mut flat_newtypes = Self::build_module_map(
                &all_newtypes,
                module_source,
                &imported_type_sources,
                &import_original_names,
            );
            let mut flat_struct_fields = Self::build_module_map(
                &all_struct_fields,
                module_source,
                &imported_type_sources,
                &import_original_names,
            );
            let flat_resource_types = Self::build_module_map(
                &all_resource_types,
                module_source,
                &imported_type_sources,
                &import_original_names,
            );

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
                                    &flat_newtypes,
                                    &flat_struct_fields,
                                    &flat_resource_types,
                                )
                            } else {
                                Self::resolve_type_static_with_params(
                                    &field.ty,
                                    &mut type_table.borrow_mut(),
                                    &flat_newtypes,
                                    &flat_struct_fields,
                                    &flat_resource_types,
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
                        // Update the nested map entry with actual fields
                        let info = StructFieldInfo {
                            module_source: module_source.clone(),
                            fields,
                            type_param_bounds,
                        };
                        all_struct_fields
                            .entry(module_source.clone())
                            .or_default()
                            .insert(struct_decl.name.clone(), info.clone());
                        // Also update flat map for subsequent items in this module
                        flat_struct_fields.insert(struct_decl.name.clone(), info);
                    }
                    Item::Type(newtype_decl) => {
                        // Resolve the base type
                        let base_type_id = Self::resolve_type_static(
                            &newtype_decl.ty,
                            &mut type_table.borrow_mut(),
                            &flat_newtypes,
                            &flat_struct_fields,
                            &flat_resource_types,
                        );
                        // Create a newtype wrapping the base type
                        let newtype_id = type_table.borrow_mut().make_newtype(
                            newtype_decl.name.clone(),
                            module_source.clone(),
                            base_type_id,
                        );
                        all_newtypes
                            .entry(module_source.clone())
                            .or_default()
                            .insert(newtype_decl.name.clone(), newtype_id);
                        // Also update flat map for subsequent items in this module
                        flat_newtypes.insert(newtype_decl.name.clone(), newtype_id);
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
                                    &flat_newtypes,
                                    &flat_struct_fields,
                                    &flat_resource_types,
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
                        all_variant_cases
                            .entry(module_source.clone())
                            .or_default()
                            .insert(
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
                        all_enum_cases
                            .entry(module_source.clone())
                            .or_default()
                            .insert(
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
        let sorted_sources =
            Self::topological_sort_modules(modules, &all_struct_fields, &type_table.borrow());

        let (wasi_registry, _) = WasiRegistry::build_from_stdlib();
        let builtin_registry = BuiltinRegistry::build_from_stdlib(&type_table);

        // Second pass: resolve each module with per-module function_return_types and imports
        for module_source in &sorted_sources {
            let module = modules.get(module_source).expect("module should exist");

            // Build imported type sources and module-specific flat maps for this module
            let (imported_type_sources, import_original_names) =
                Self::build_imported_type_sources(module, module_source);
            let newtypes = Self::build_module_map(
                &all_newtypes,
                module_source,
                &imported_type_sources,
                &import_original_names,
            );
            let struct_fields = Self::build_module_map(
                &all_struct_fields,
                module_source,
                &imported_type_sources,
                &import_original_names,
            );
            let variant_cases = Self::build_module_map(
                &all_variant_cases,
                module_source,
                &imported_type_sources,
                &import_original_names,
            );
            let enum_cases = Self::build_module_map(
                &all_enum_cases,
                module_source,
                &imported_type_sources,
                &import_original_names,
            );
            let resource_types = Self::build_module_map(
                &all_resource_types,
                module_source,
                &imported_type_sources,
                &import_original_names,
            );

            // Build function_return_types for this module only
            // (functions defined in this module)
            let mut function_return_types = IndexMap::new();
            for item in &module.items {
                if let Item::Function(func) = item {
                    let return_type = if let Some(ret_ty) = &func.return_type {
                        Self::resolve_type_static(
                            ret_ty,
                            &mut type_table.borrow_mut(),
                            &newtypes,
                            &struct_fields,
                            &resource_types,
                        )
                    } else {
                        TypeTable::UNIT
                    };
                    function_return_types.insert(func.name.clone(), return_type);
                }
            }

            // Collect imported function names from this module's use declarations
            let mut imported_functions = IndexSet::new();
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
                type_table: Rc::clone(&type_table),
                symbols,
                loaded_modules: modules,
                newtypes,
                struct_fields,
                variant_cases,
                enum_cases,
                resource_types,
                all_newtypes: all_newtypes.clone(),
                all_struct_fields: all_struct_fields.clone(),
                all_variant_cases: all_variant_cases.clone(),
                all_enum_cases: all_enum_cases.clone(),
                all_resource_types: all_resource_types.clone(),
                function_return_types,
                imported_functions,
                logger,
                current_module_source: ModuleSource::entry_point_with_filename("<uninitialized>"), // Set in resolve_module
                current_module_items: Vec::new(), // Set in resolve_module
                current_type_params: IndexMap::new(),
                current_type_param_bounds: IndexMap::new(),
                generic_struct_names: IndexSet::new(),
                generic_function_params: IndexMap::new(),
                generic_method_params: IndexMap::new(),
                current_associated_type_bindings: IndexMap::new(),
                current_self_type: None,
                wasi_registry,
                builtin_registry: &builtin_registry,
                current_module_globals: IndexMap::new(),
                imported_globals: IndexMap::new(),
                associated_constants: IndexMap::new(),
                module_type_maps_cache: IndexMap::new(),
            };

            // Errors are emitted to the logger; if resolve_module returns Bail,
            // we continue to resolve remaining modules to collect more errors
            if let Ok(tir_module) = resolver.resolve_module(module, module_source.clone()) {
                result.insert(module_source.clone(), tir_module);
            }
        }

        logger.ok_or_bail(result)
    }

    /// Build a module-specific flat map from per-module entries.
    ///
    /// Priority: current module > imported types > any available definition.
    fn build_module_map<V: Clone>(
        per_module: &IndexMap<ModuleSource, IndexMap<String, V>>,
        current_module: &ModuleSource,
        imported_type_sources: &IndexMap<String, ModuleSource>,
        import_original_names: &IndexMap<String, String>,
    ) -> IndexMap<String, V> {
        let mut result = IndexMap::new();
        // First: add all entries from all modules (arbitrary winner for conflicts)
        for name_map in per_module.values() {
            for (name, value) in name_map {
                result.entry(name.clone()).or_insert_with(|| value.clone());
            }
        }
        // Second: override with imported modules' types
        for (local_name, import_src) in imported_type_sources {
            // Use original name to look up in the source module (handles `use { Foo as Bar }`)
            let lookup_name = import_original_names.get(local_name).unwrap_or(local_name);
            if let Some(name_map) = per_module.get(import_src)
                && let Some(value) = name_map.get(lookup_name)
            {
                result.insert(local_name.clone(), value.clone());
            }
        }
        // Third: override with current module's types (highest priority)
        if let Some(name_map) = per_module.get(current_module) {
            for (name, value) in name_map {
                result.insert(name.clone(), value.clone());
            }
        }
        result
    }

    /// Build a map of imported names to their source modules from use declarations.
    /// Build a mapping from local import names to their source modules and original names.
    ///
    /// Returns `(local_name -> module_source, local_name -> original_name)`.
    /// The `original_name` is different from `local_name` when `use { Foo as Bar }` is used.
    fn build_imported_type_sources(
        module: &Module,
        from_module: &ModuleSource,
    ) -> (IndexMap<String, ModuleSource>, IndexMap<String, String>) {
        let mut sources = IndexMap::new();
        let mut original_names = IndexMap::new();
        for item in &module.items {
            if let Item::Use(use_decl) = item {
                let source = name::resolve_import(from_module, &use_decl.source);
                for use_item in &use_decl.items {
                    match use_item {
                        ast::UseItem::Simple { name, alias } => {
                            let local_name = alias.as_ref().unwrap_or(name);
                            sources.insert(local_name.clone(), source.clone());
                            if alias.is_some() {
                                original_names.insert(local_name.clone(), name.clone());
                            }
                        }
                        ast::UseItem::EffectFunctions { .. } => {}
                    }
                }
            }
        }
        (sources, original_names)
    }

    /// Lazily build and cache per-module type maps for cross-module type resolution.
    ///
    /// Must be called before borrowing `loaded_modules` for the same module,
    /// so the cache is populated without borrow conflicts. After calling this,
    /// use `self.module_type_maps_cache.remove(module_source)` to get the cached
    /// maps and swap them into the resolver's active maps.
    fn ensure_module_maps_cached(&mut self, module_source: &ModuleSource) {
        if self.module_type_maps_cache.contains_key(module_source) {
            return;
        }
        let Some(module) = self.loaded_modules.get(module_source) else {
            return;
        };
        let (imported_sources, import_names) =
            Self::build_imported_type_sources(module, module_source);
        let maps = ModuleTypeMaps {
            struct_fields: Self::build_module_map(
                &self.all_struct_fields,
                module_source,
                &imported_sources,
                &import_names,
            ),
            variant_cases: Self::build_module_map(
                &self.all_variant_cases,
                module_source,
                &imported_sources,
                &import_names,
            ),
            enum_cases: Self::build_module_map(
                &self.all_enum_cases,
                module_source,
                &imported_sources,
                &import_names,
            ),
            newtypes: Self::build_module_map(
                &self.all_newtypes,
                module_source,
                &imported_sources,
                &import_names,
            ),
            resource_types: Self::build_module_map(
                &self.all_resource_types,
                module_source,
                &imported_sources,
                &import_names,
            ),
        };
        self.module_type_maps_cache
            .insert(module_source.clone(), maps);
    }

    /// Topologically sort modules based on struct field type dependencies.
    ///
    /// Recursively collect cross-module struct/variant dependencies from a type.
    /// Unwraps all wrapper types (`Ref`, `MutRef`, `Option`, `GenericInstance`, `Tuple`, etc.)
    /// to find underlying Struct/Variant types defined in other modules.
    fn collect_cross_module_deps(
        type_id: TypeId,
        type_table: &TypeTable,
        out: &mut Vec<(String, ModuleSource)>,
    ) {
        match type_table.get(type_id) {
            ResolvedType::Struct {
                name,
                module_source,
                ..
            }
            | ResolvedType::Variant {
                name,
                module_source,
                ..
            } => {
                out.push((name.clone(), module_source.clone()));
            }
            ResolvedType::Ref(inner)
            | ResolvedType::MutRef(inner)
            | ResolvedType::Option(inner)
            | ResolvedType::BuiltinArray(inner)
            | ResolvedType::Stream(inner)
            | ResolvedType::Future(inner)
            | ResolvedType::Reactive(inner) => {
                Self::collect_cross_module_deps(*inner, type_table, out);
            }
            ResolvedType::GenericInstance { type_args, .. } => {
                for arg in type_args {
                    Self::collect_cross_module_deps(*arg, type_table, out);
                }
            }
            ResolvedType::Tuple(elems) => {
                for elem in elems {
                    Self::collect_cross_module_deps(*elem, type_table, out);
                }
            }
            _ => {}
        }
    }

    /// A module A depends on module B if A contains a struct with a field whose type
    /// is a struct defined in B. This ensures that when we register struct types in
    /// codegen, dependency structs are registered before the structs that reference them.
    fn topological_sort_modules(
        modules: &IndexMap<ModuleSource, Module>,
        all_struct_fields: &IndexMap<ModuleSource, IndexMap<String, StructFieldInfo>>,
        type_table: &TypeTable,
    ) -> Vec<ModuleSource> {
        // Collect and sort sources for deterministic ordering
        let mut sources: Vec<&ModuleSource> = modules.keys().collect();
        sources.sort_by_key(std::string::ToString::to_string);
        let source_to_idx: IndexMap<&ModuleSource, usize> =
            sources.iter().enumerate().map(|(i, s)| (*s, i)).collect();

        // Track dependency counts directly (no need for full dependency sets)
        let mut dependency_count: Vec<usize> = vec![0; sources.len()];
        // Track which edges we've already added to avoid duplicates
        let mut seen_edges: IndexSet<(usize, usize)> = IndexSet::new();
        // Build reverse graph: dependents[i] = modules that depend on module i
        let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); sources.len()];

        // Analyze struct fields to find cross-module dependencies.
        // Recursively unwrap wrapper types (Ref, MutRef, Option, GenericInstance,
        // Tuple, etc.) to detect dependencies through any nesting level.
        for (module_src, name_map) in all_struct_fields {
            let Some(&from_idx) = source_to_idx.get(module_src) else {
                continue;
            };
            for (struct_name, info) in name_map {
                for (_field_name, field_type_id) in &info.fields {
                    let mut dep_sources = Vec::new();
                    Self::collect_cross_module_deps(*field_type_id, type_table, &mut dep_sources);
                    for (ref_name, ref_module_source) in dep_sources {
                        // Skip self-references (same struct or same module)
                        if ref_name == *struct_name || ref_module_source == *module_src {
                            continue;
                        }
                        if let Some(&to_idx) = source_to_idx.get(&ref_module_source) {
                            // from_idx depends on to_idx (dependency edge)
                            if seen_edges.insert((from_idx, to_idx)) {
                                dependency_count[from_idx] += 1;
                                dependents[to_idx].push(from_idx);
                            }
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

        let mut sorted_indices = Vec::with_capacity(sources.len());
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

        // Cycle detection with warning (O(n) using IndexSet)
        if sorted_indices.len() < sources.len() {
            let sorted_set: IndexSet<usize> = sorted_indices.iter().copied().collect();
            let in_cycle: Vec<usize> = (0..sources.len())
                .filter(|i| !sorted_set.contains(i))
                .collect();
            let cycle_modules: Vec<_> = in_cycle.iter().map(|&i| sources[i].to_string()).collect();
            eprintln!(
                "Warning: circular struct dependencies detected among modules: {}",
                cycle_modules.join(", ")
            );
            // Append remaining in deterministic order (already sorted by index)
            sorted_indices.extend(in_cycle);
        }

        // Convert indices back to sources
        sorted_indices.iter().map(|&i| sources[i].clone()).collect()
    }

    /// Static version of `resolve_type` for use before the resolver is fully constructed
    fn resolve_type_static(
        ty: &Type,
        type_table: &mut TypeTable,
        newtypes: &IndexMap<String, TypeId>,
        struct_fields: &IndexMap<String, StructFieldInfo>,
        resource_types: &IndexMap<String, ResourceInfo>,
    ) -> TypeId {
        match ty {
            Type::Named(named) => {
                // Check newtypes first
                if let Some(&alias_type_id) = newtypes.get(&named.name) {
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
                        } else if let Some(info) = resource_types.get(&named.name) {
                            type_table.make_resource(named.name.clone(), info.module_source.clone())
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
                        newtypes,
                        struct_fields,
                        resource_types,
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
                                    newtypes,
                                    struct_fields,
                                    resource_types,
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
                let inner_type = Self::resolve_type_static(
                    inner,
                    type_table,
                    newtypes,
                    struct_fields,
                    resource_types,
                );
                type_table.make_ref(inner_type)
            }
            Type::MutReference(inner) => {
                let inner_type = Self::resolve_type_static(
                    inner,
                    type_table,
                    newtypes,
                    struct_fields,
                    resource_types,
                );
                type_table.make_mut_ref(inner_type)
            }
            Type::NamespacedGeneric(namespaced) => {
                // Handle builtin::array<T>
                if namespaced.namespace == "builtin"
                    && namespaced.name == "array"
                    && let Some(elem_ty) = namespaced.args.first()
                {
                    let elem = Self::resolve_type_static(
                        elem_ty,
                        type_table,
                        newtypes,
                        struct_fields,
                        resource_types,
                    );
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
        newtypes: &IndexMap<String, TypeId>,
        struct_fields: &IndexMap<String, StructFieldInfo>,
        resource_types: &IndexMap<String, ResourceInfo>,
        type_params: &[String],
    ) -> TypeId {
        match ty {
            Type::Named(named) => {
                // Check newtypes first
                if let Some(&alias_type_id) = newtypes.get(&named.name) {
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
                        } else if let Some(info) = resource_types.get(&named.name) {
                            type_table.make_resource(named.name.clone(), info.module_source.clone())
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
                        newtypes,
                        struct_fields,
                        resource_types,
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
                                    newtypes,
                                    struct_fields,
                                    resource_types,
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
                    newtypes,
                    struct_fields,
                    resource_types,
                    type_params,
                );
                type_table.make_ref(inner_type)
            }
            Type::MutReference(inner) => {
                let inner_type = Self::resolve_type_static_with_params(
                    inner,
                    type_table,
                    newtypes,
                    struct_fields,
                    resource_types,
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
                        newtypes,
                        struct_fields,
                        resource_types,
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
        for (module_source, loaded_module) in self.loaded_modules {
            for item in &loaded_module.items {
                if let Item::Type(newtype_decl) = item {
                    // Only add if not already present (main module takes priority)
                    if !self.newtypes.contains_key(&newtype_decl.name) {
                        // Resolve the base type
                        let base_type_id = self.resolve_type(&newtype_decl.ty);
                        // Create a newtype wrapping the base type
                        let newtype_id = self.type_table.borrow_mut().make_newtype(
                            newtype_decl.name.clone(),
                            module_source.clone(),
                            base_type_id,
                        );
                        self.newtypes.insert(newtype_decl.name.clone(), newtype_id);
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
                Item::Type(newtype_decl) => {
                    // Resolve the base type
                    let base_type_id = self.resolve_type(&newtype_decl.ty);
                    // Create a newtype wrapping the base type
                    let newtype_id = self.type_table.borrow_mut().make_newtype(
                        newtype_decl.name.clone(),
                        self.current_module_source.clone(),
                        base_type_id,
                    );
                    self.newtypes.insert(newtype_decl.name.clone(), newtype_id);
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
                    let old_type_param_bounds = std::mem::take(&mut self.current_type_param_bounds);

                    // First, collect explicit type params from impl<T>
                    for (index, param) in impl_block.type_params.iter().enumerate() {
                        let type_id = self
                            .type_table
                            .borrow_mut()
                            .make_type_param(param.name.clone(), index as u32);
                        self.current_type_params
                            .insert(param.name.clone(), (index as u32, type_id));
                        if !param.bounds.is_empty() {
                            self.current_type_param_bounds
                                .insert(param.name.clone(), param.bounds.clone());
                        }
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

                        let mangled_name = MethodName::format_local(
                            &struct_name,
                            trait_name.as_deref(),
                            &method.name,
                        );
                        self.function_return_types.insert(mangled_name, return_type);
                    }

                    // Restore type parameters, bounds, and associated type bindings
                    self.current_type_params = old_type_params;
                    self.current_type_param_bounds = old_type_param_bounds;
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
            let _ = self.logger.error(TypeError::TypeMismatch {
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
        let old_type_param_bounds = std::mem::take(&mut self.current_type_param_bounds);
        let mut type_param_list = Vec::new();
        for (index, param) in func.type_params.iter().enumerate() {
            let type_id = self
                .type_table
                .borrow_mut()
                .make_type_param(param.name.clone(), index as u32);
            self.current_type_params
                .insert(param.name.clone(), (index as u32, type_id));
            if !param.bounds.is_empty() {
                self.current_type_param_bounds
                    .insert(param.name.clone(), param.bounds.clone());
            }
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
        self.current_type_param_bounds = old_type_param_bounds;

        Some(TirFunction {
            name: func.name.clone(),
            is_pub: func.is_pub,
            is_export: func.is_export,
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
            needed_copy_types: IndexSet::new(),
            // Scratch local fields - computed by lower phase
            scratch_locals: Vec::new(),
            copy_source_types: IndexSet::new(),
            indirect_call_counts: IndexMap::new(),
            match_scrutinee_types: Vec::new(),
            let_pattern_types: Vec::new(),
            is_cm_adapter: false,
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
            is_pub: false,    // Tests are not public
            is_export: false, // Tests are not world exports
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
            needed_copy_types: IndexSet::new(),
            scratch_locals: Vec::new(),
            copy_source_types: IndexSet::new(),
            indirect_call_counts: IndexMap::new(),
            match_scrutinee_types: Vec::new(),
            let_pattern_types: Vec::new(),
            is_cm_adapter: false,
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
        let old_type_param_bounds = std::mem::take(&mut self.current_type_param_bounds);
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

        // Populate bounds from the impl block's type_params
        // (inherited from outer scope - second-pass sets these up)
        // Re-read from current_type_param_bounds which was set by the caller
        // Actually, the caller (second-pass) already set up bounds, but we took them.
        // We need to restore them from old_type_param_bounds temporarily... NO.
        // The caller sets up bounds BEFORE calling resolve_method, so old_type_param_bounds
        // contains the caller's bounds. We should use those as base and add method-level bounds.
        self.current_type_param_bounds = old_type_param_bounds.clone();

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
            if !param.bounds.is_empty() {
                self.current_type_param_bounds
                    .insert(param.name.clone(), param.bounds.clone());
            }
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
        let mangled_name = MethodName::format_local(struct_name, trait_name, &func.name);
        self.function_return_types
            .insert(mangled_name.clone(), return_type);

        // Display name for #function: StructName::method_name
        let display_name = MethodName::format_local(struct_name, None, &func.name);
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

        // Restore previous type params scope and bounds
        self.current_type_params = old_type_params;
        self.current_type_param_bounds = old_type_param_bounds;

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
            is_export: false, // Methods are not world exports
            type_params,
            impl_type_params, // Type params from impl block (e.g., T from impl Counter<T>)
            monomorph_info: None, // Not from monomorphization
            method_info: Some(LocalMethodName {
                struct_name: struct_name.to_string(),
                base_struct_name: struct_name.to_string(),
                trait_name: trait_name.map(String::from),
                method_name: func.name.clone(),
                method_type_args: vec![],
                is_type_param_receiver: false,
            }),
            params,
            return_type,
            effects: func.effects.clone(),
            body,
            span: func.span,
            local_count: ctx.next_local,
            local_types: ctx.local_types,
            address_taken_locals: ctx.address_taken_locals,
            needed_copy_types: IndexSet::new(),
            scratch_locals: Vec::new(),
            copy_source_types: IndexSet::new(),
            indirect_call_counts: IndexMap::new(),
            match_scrutinee_types: Vec::new(),
            let_pattern_types: Vec::new(),
            is_cm_adapter: false,
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

    /// Resolve a block with expected type for its result expression.
    /// The last expression statement (if any) is resolved with the expected type
    /// for literal coercion, while all other statements are resolved normally.
    fn resolve_block_with_expected_type(
        &mut self,
        block: &Block,
        ctx: &mut FunctionContext,
        expected_type: TypeId,
    ) -> TirBlock {
        ctx.enter_scope();
        let len = block.stmts.len();
        let mut stmts = Vec::new();
        for (i, s) in block.stmts.iter().enumerate() {
            if i == len - 1
                && let Stmt::Expr(expr_stmt) = s
            {
                let expr =
                    self.resolve_expr_with_expected_type(&expr_stmt.expr, ctx, Some(expected_type));
                stmts.push(TirStmt::new(TirStmtKind::Expr(expr), expr_stmt.span));
                continue;
            }
            stmts.extend(self.resolve_stmt(s, ctx));
        }
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
        ctx.active_labels.push(labeled_block.label.clone());
        // resolve_block already handles scope entry/exit
        let block = self.resolve_block(&labeled_block.block, ctx);
        ctx.active_labels.pop();

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
                                let _ = self.logger.error(TypeError::TypeMismatch {
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
                                    let _ = self.logger.error(TypeError::TypeMismatch {
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
                            let _ = self.logger.error(TypeError::TypeMismatch {
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
                        let _ = self.logger.error(TypeError::TypeMismatch {
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
                let _ = self.logger.error(TypeError::TypeMismatch {
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
                let _ = self.logger.error(TypeError::InvalidPattern {
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
                        let _ = self.logger.error(TypeError::TypeMismatch {
                            expected: "tuple type".to_string(),
                            found: type_table.type_name(type_id),
                            span,
                        });
                        vec![TypeTable::UNKNOWN; patterns.len()]
                    }
                };

                // Check length
                if patterns.len() != elem_types.len() {
                    let _ = self.logger.error(TypeError::TypeMismatch {
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
                let _ = self.logger.error(TypeError::InvalidPattern {
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
                            let _ = self.logger.error(TypeError::InvalidPattern {
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

                // Handle enum types (no payload, just discriminant matching)
                if let ResolvedType::Enum { name, .. } = &resolved_type {
                    if !bindings.is_empty() {
                        let _ = self.logger.error(TypeError::InvalidPattern {
                            message: format!("enum case `{variant_name}` does not have a payload"),
                            span: *span,
                        });
                    }
                    // Look up the enum case index
                    if let Some(enum_info) = self.enum_cases.get(name) {
                        if let Some(case_data) =
                            enum_info.cases.iter().find(|c| c.name == *variant_name)
                        {
                            return TirPattern::Enum {
                                enum_type: scrutinee_type,
                                case_name: variant_name.clone(),
                                case_index: case_data.index,
                            };
                        }
                        let _ = self.logger.error(TypeError::TypeMismatch {
                            expected: format!(
                                "one of: {}",
                                enum_info
                                    .cases
                                    .iter()
                                    .map(|c| c.name.as_str())
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            ),
                            found: variant_name.clone(),
                            span: *span,
                        });
                        return TirPattern::Wildcard;
                    }
                    let _ = self.logger.error(TypeError::TypeMismatch {
                        expected: format!("enum type `{name}`"),
                        found: "unknown enum".to_string(),
                        span: *span,
                    });
                    return TirPattern::Wildcard;
                }

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
                            let _ = self.logger.error(TypeError::TypeMismatch {
                                expected: "variant type".to_string(),
                                found: name.clone(),
                                span: *span,
                            });
                            TypeTable::UNKNOWN
                        }
                    }
                    _ => {
                        let _ = self.logger.error(TypeError::TypeMismatch {
                            expected: "variant or enum type".to_string(),
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
            let _ = self.logger.error(TypeError::TypeMismatch {
                expected: format!("valid case of variant {variant_name}"),
                found: case_name.to_string(),
                span,
            });
        } else {
            let _ = self.logger.error(TypeError::TypeMismatch {
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

        // Validate that the target label exists
        if let Some(label) = &break_stmt.label
            && !ctx.active_labels.iter().any(|l| l == label)
        {
            let _ = self.logger.error(TypeError::UnknownIdentifier {
                name: format!("labeled break target not found: {label}"),
                span: break_stmt.span,
            });
        }

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
                ctx.active_labels.push(lb.label.clone());

                ctx.enter_scope();
                let tir_block = self.resolve_block(&lb.block, ctx);
                ctx.exit_scope();

                // Pop the target and determine the result type from break statements
                ctx.active_labels.pop();
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
    #[allow(clippy::cast_precision_loss, clippy::cast_sign_loss)]
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
    #[allow(clippy::cast_precision_loss)]
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
    #[allow(clippy::cast_sign_loss)]
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
                            let _ = self.logger.error(TypeError::InvalidLiteral {
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
                            let _ = self.logger.error(TypeError::InvalidLiteral {
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

        // Check for associated constants (e.g., f64::PI, i32::MAX)
        if let Some((const_ty, const_expr)) = self.associated_constants.get(&ident.name).cloned() {
            let type_id = self.resolve_type(&const_ty);
            let resolved = self.resolve_expr_with_expected_type(&const_expr, ctx, Some(type_id));
            return TirExpr::new(resolved.kind, type_id, ident.span);
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
                        let _ = self.logger.error(TypeError::ArgumentCountMismatch {
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
                    .map(|s| s.module_source.clone())
                    .unwrap_or_else(|| self.current_module_source.clone())
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
        // These are defined in core:internal and re-exported by core:prelude
        if matches!(ident.name.as_str(), "panic" | "unreachable") {
            return TirExpr::new(
                TirExprKind::Global {
                    module_source: ModuleSource::core("internal"),
                    name: ident.name.clone(),
                },
                TypeTable::UNKNOWN,
                ident.span,
            );
        }

        // Unknown variable - report error
        let _ = self.logger.error(TypeError::UnknownIdentifier {
            name: ident.name.clone(),
            span: ident.span,
        });
        TirExpr::new(TirExprKind::Unit, TypeTable::ERROR, ident.span)
    }

    /// Resolve a binary expression
    fn resolve_binary(&mut self, binary: &ast::BinaryExpr, ctx: &mut FunctionContext) -> TirExpr {
        self.resolve_binary_with_expected_type(binary, ctx, None)
    }

    fn resolve_binary_with_expected_type(
        &mut self,
        binary: &ast::BinaryExpr,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
    ) -> TirExpr {
        // Bidirectional coercion: if one operand is a numeric literal and the other is not,
        // resolve the non-literal first and use its type to coerce the literal
        let left_is_numeric_literal = self.is_numeric_literal(&binary.left);
        let right_is_numeric_literal = self.is_numeric_literal(&binary.right);

        let (left, right) = if left_is_numeric_literal && !right_is_numeric_literal {
            // Resolve right first, then coerce left to right's type
            let right = self.resolve_expr(&binary.right, ctx);
            let coerce_type = if self.type_table.borrow().is_numeric(right.type_id) {
                Some(right.type_id)
            } else {
                None
            };
            let left = self.resolve_expr_with_expected_type(&binary.left, ctx, coerce_type);
            (left, right)
        } else if right_is_numeric_literal && !left_is_numeric_literal {
            // Resolve left first, then coerce right to left's type
            let left = self.resolve_expr(&binary.left, ctx);
            let coerce_type = if self.type_table.borrow().is_numeric(left.type_id) {
                Some(left.type_id)
            } else {
                None
            };
            let right = self.resolve_expr_with_expected_type(&binary.right, ctx, coerce_type);
            (left, right)
        } else if left_is_numeric_literal && right_is_numeric_literal {
            // Both literals - use expected type from context (e.g., assignment target)
            let left = self.resolve_expr_with_expected_type(&binary.left, ctx, expected_type);
            let right = self.resolve_expr_with_expected_type(&binary.right, ctx, expected_type);
            (left, right)
        } else {
            // Both non-literals - resolve normally
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
                // Handle Eq trait (== and !=)
                if matches!(binary.op, BinaryOp::Eq | BinaryOp::NotEq)
                    && let Some(trait_info) = self.find_eq_trait_impl(&struct_name, left.type_id)
                {
                    let receiver = self.adjust_receiver_for_self_kind(
                        left.clone(),
                        trait_info.self_kind,
                        binary.span,
                    );

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

                    let mangled_method_name =
                        MethodName::format_local(&struct_name, Some(&trait_info.trait_name), "eq");

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
                                    "eq".to_string(),
                                )),
                            },
                            type_args: vec![],
                            args: vec![arg_ref],
                        },
                        TypeTable::BOOL,
                        binary.span,
                    );

                    // Apply negation for !=
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

                // Handle Ord trait (<, >, <=, >=)
                // Ord::cmp returns Ordering enum with discriminants: Less=0, Equal=1, Greater=2
                if matches!(
                    binary.op,
                    BinaryOp::Lt | BinaryOp::Gt | BinaryOp::LtEq | BinaryOp::GtEq
                ) && let Some(trait_info) = self.find_ord_trait_impl(&struct_name, left.type_id)
                {
                    let receiver = self.adjust_receiver_for_self_kind(
                        left.clone(),
                        trait_info.self_kind,
                        binary.span,
                    );

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

                    // Get Ordering type for cmp return value
                    let ordering_type_id =
                        self.type_table.borrow_mut().intern(ResolvedType::Enum {
                            name: "Ordering".to_string(),
                            module_source: ModuleSource::core("prelude"),
                        });

                    let mangled_method_name =
                        MethodName::format_local(&struct_name, Some(&trait_info.trait_name), "cmp");

                    let cmp_call = TirExpr::new(
                        TirExprKind::MethodCall {
                            receiver: Box::new(receiver),
                            func: FunctionRef::External {
                                module_source: ModuleSource::core("prelude"),
                                name: mangled_method_name,
                                monomorph_info: None,
                                method_info: Some(LocalMethodName::new(
                                    struct_name.clone(),
                                    Some(trait_info.trait_name.clone()),
                                    "cmp".to_string(),
                                )),
                            },
                            type_args: vec![],
                            args: vec![arg_ref],
                        },
                        ordering_type_id,
                        binary.span,
                    );

                    // Determine comparison operator and Ordering variant:
                    // < : cmp(a, b) == Ordering::Less
                    // > : cmp(a, b) == Ordering::Greater
                    // <= : cmp(a, b) != Ordering::Greater
                    // >= : cmp(a, b) != Ordering::Less
                    let (compare_op, case_name, case_index): (TirBinaryOp, &str, u32) =
                        match binary.op {
                            BinaryOp::Lt => (TirBinaryOp::Eq, "Less", 0),
                            BinaryOp::Gt => (TirBinaryOp::Eq, "Greater", 2),
                            BinaryOp::LtEq => (TirBinaryOp::NotEq, "Greater", 2),
                            BinaryOp::GtEq => (TirBinaryOp::NotEq, "Less", 0),
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
                        binary.span,
                    );

                    return TirExpr::new(
                        TirExprKind::Binary {
                            op: compare_op,
                            left: Box::new(cmp_call),
                            right: Box::new(ordering_variant),
                        },
                        TypeTable::BOOL,
                        binary.span,
                    );
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

                    let mangled_method_name = MethodName::format_local(
                        &struct_name,
                        Some(&trait_info.trait_name),
                        method_name,
                    );

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
                    let mangled_method_name = MethodName::format_local(
                        &struct_name,
                        Some(&trait_info.trait_name),
                        method_name,
                    );

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
                let _ = self.logger.error(TypeError::TypeMismatch {
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
            let _ = self.logger.error(TypeError::TypeMismatch {
                expected: "mutable variable".to_string(),
                found: format!("immutable variable '{name}'"),
                span: unary.span,
            });
        }

        // Reject &mut on struct field access when the field is a primitive type.
        // In Wasm GC, struct.get returns a value copy for primitives, so &mut field
        // creates a disconnected Box — mutations don't propagate back to the struct.
        // For GC reference types (struct, String, Array, etc.), struct.get returns
        // the shared reference, so &mut field works correctly.
        if unary.op == UnaryOp::MutRef && matches!(&expr.kind, TirExprKind::FieldAccess { .. }) {
            let field_type = self.type_table.borrow().get(expr.type_id).clone();
            let base_type = self
                .type_table
                .borrow()
                .get(
                    self.type_table
                        .borrow()
                        .get_ultimate_base_type(expr.type_id),
                )
                .clone();
            if matches!(field_type, ResolvedType::Primitive(_))
                || matches!(base_type, ResolvedType::Primitive(_))
            {
                let _ = self.logger.error(TypeError::CannotAssign {
                    message: "cannot take mutable reference to primitive struct field; use the struct reference directly".to_string(),
                    span: unary.span,
                });
            }
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

                    let mangled_method_name =
                        MethodName::format_local(&struct_name, Some(&trait_info.trait_name), "neg");

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

                    let mangled_method_name = MethodName::format_local(
                        &struct_name,
                        Some(&trait_info.trait_name),
                        "bitnot",
                    );

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
                    let neg_value = (*value as i64).wrapping_neg().cast_unsigned();
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
                        let neg_value = (*value as i64).wrapping_neg().cast_unsigned();
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
                    let _ = self.logger.error(TypeError::TypeMismatch {
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
                        let mangled_method_name = MethodName::format_local(
                            &struct_name,
                            Some(&trait_info.trait_name),
                            "index_assign",
                        );

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
        // Use target's type as expected type for value resolution
        // This enables coercion of empty array literals [] to the field's Array<T> type
        let value = self.resolve_expr_with_expected_type(&assign.value, ctx, Some(target.type_id));

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
                    let _ = self.logger.error(TypeError::CannotAssign {
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
                    TypeTable::UNIT,
                    assign.span,
                );
            }
        }

        // Validate that the target is a valid l-value
        let is_valid_lvalue = match &target.kind {
            TirExprKind::Local { .. } => true,
            TirExprKind::FieldAccess { .. } => true,
            TirExprKind::Index { .. } => true,
            // Dereference is a valid l-value only through mutable reference
            TirExprKind::Unary {
                op: TirUnaryOp::Deref,
                expr,
                ..
            } => {
                let inner_type = self.type_table.borrow().get(expr.type_id).clone();
                if matches!(inner_type, ResolvedType::Ref(_)) {
                    let _ = self.logger.error(TypeError::CannotAssign {
                        message: "cannot assign through immutable reference".to_string(),
                        span: assign.target.span(),
                    });
                    false
                } else {
                    true
                }
            }
            _ => false,
        };

        if !is_valid_lvalue {
            // Report error for invalid assignment target
            let _ = self.logger.error(TypeError::CannotAssign {
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
            TypeTable::UNIT,
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
        // callee_module_source is None for local calls (uses current module), Some for external calls
        let (callee_module_source, func_name, is_known) = match &call.callee {
            Expr::Ident(ident) => {
                // Check for qualified name with :: (e.g., "Stdout::write_via_stream")
                // Parser creates a single ident for Effect::operation syntax
                if let Some(pos) = ident.name.find("::") {
                    let prefix = &ident.name[..pos];
                    let suffix = &ident.name[pos + 2..];

                    // Builtin functions: resolve through core:builtin module
                    if prefix == "builtin" {
                        (
                            Some(ModuleSource::core("builtin")),
                            suffix.to_string(),
                            true,
                        )
                    }
                    // Check if this is a static method call (Type::method)
                    // Static methods are registered with mangled names "Type::method"
                    else if self.is_static_method(prefix, suffix) {
                        // Return as a static method call - will be converted to StaticCall below
                        let mangled_name = MethodName::format_local(prefix, None, suffix);
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
                                let _ = self.logger.error(TypeError::ArgumentCountMismatch {
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
                            let _ = self.logger.error(TypeError::UnknownFunction {
                                name: format!("{prefix}::{suffix}"),
                                span: call.span,
                            });
                            return TirExpr::new(TirExprKind::Unit, TypeTable::ERROR, call.span);
                        }
                    }
                    // Effect operations and other qualified calls - always allowed
                    // (validated by effect system/codegen)
                    else {
                        // Effect-like modules (e.g., "Stdout") use Local module source
                        (
                            Some(ModuleSource::Local {
                                path: prefix.to_string(),
                            }),
                            suffix.to_string(),
                            true,
                        )
                    }
                }
                // Check if it's a local function (defined in this module) or
                // a built-in type constructor (Ok, Err, Some, None)
                else if self.function_return_types.contains_key(&ident.name)
                    || matches!(ident.name.as_str(), "Ok" | "Err" | "Some" | "None")
                {
                    (None, ident.name.clone(), true)
                }
                // Check for prelude functions (panic, unreachable)
                // These are defined in core:internal and re-exported by core:prelude
                else if matches!(ident.name.as_str(), "panic" | "unreachable") {
                    (
                        Some(ModuleSource::core("internal")),
                        ident.name.clone(),
                        true,
                    )
                }
                // Check if this is an imported function (per-module imports)
                else if self.imported_functions.contains(&ident.name) {
                    // Get module source from symbol table for codegen
                    if let Some(symbol) = self.symbols.lookup(&ident.name) {
                        (
                            Some(symbol.module_source.clone()),
                            symbol.name.clone(),
                            true,
                        )
                    } else {
                        // Imported but not in symbols - shouldn't happen but allow
                        (None, ident.name.clone(), true)
                    }
                } else {
                    // Unknown function - will report error
                    (None, ident.name.clone(), false)
                }
            }
            Expr::FieldAccess(field_access) => {
                // e.g., Stdout.write (unlikely but possible)
                // These are always considered known - validated elsewhere
                if let Expr::Ident(ident) = &field_access.expr {
                    (
                        Some(ModuleSource::Local {
                            path: ident.name.clone(),
                        }),
                        field_access.field.clone(),
                        true,
                    )
                } else {
                    (None, String::from("unknown"), false)
                }
            }
            _ => (None, String::from("unknown"), false),
        };

        // Report error for unknown functions
        if !is_known {
            let _ = self.logger.error(TypeError::UnknownFunction {
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

        // For local function calls (None), use the current module source
        // to ensure DCE and codegen can find the function correctly
        let callee_module =
            callee_module_source.unwrap_or_else(|| self.current_module_source.clone());

        // Check trait bounds on function type arguments
        if !type_args.is_empty() {
            self.check_function_type_arg_bounds(&callee_module, &func_name, &type_args, call.span);
        }

        // Look up function return type
        let mut return_type = self.lookup_function_return_type(&callee_module, &func_name);

        // If we have explicit type args, substitute type parameters in the return type
        if !type_args.is_empty() {
            return_type = self.substitute_type_params(return_type, &type_args);
        }

        TirExpr::new(
            TirExprKind::Call {
                func: FunctionRef::External {
                    module_source: callee_module,
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
    fn lookup_function_return_type(
        &mut self,
        callee_module: &ModuleSource,
        func_name: &str,
    ) -> TypeId {
        // Handle builtin functions
        if callee_module.is_core_builtin() {
            return self.get_builtin_return_type(func_name);
        }
        // Legacy: builtin::name pattern
        if let Some(builtin_name) = func_name.strip_prefix("builtin::") {
            return self.get_builtin_return_type(builtin_name);
        }

        // Handle WASI effect operations (e.g., Environment::get_arguments)
        if callee_module.is_effect_like()
            && let Some(effect_name) = callee_module.effect_name()
            && let Some(return_type) = self.get_wasi_effect_return_type(&effect_name, func_name)
        {
            return return_type;
        }

        // First, try local functions (entry point module)
        if callee_module.is_entry_point()
            && let Some(&return_type) = self.function_return_types.get(func_name)
        {
            return return_type;
        }

        // Try looking up in loaded modules
        if !callee_module.is_entry_point() {
            // Clone the return type AST and type params to avoid borrow issues
            let func_info = self.loaded_modules.get(callee_module).and_then(|module| {
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

                // Resolve the return type in the callee module's context, not the caller's.
                // Build the callee module's flat maps so that type names resolve to the
                // callee's types, not the caller's (which may have same-named different types).
                let callee_module_ast = self.loaded_modules.get(callee_module);
                let (callee_imported, callee_original_names) = callee_module_ast.map_or_else(
                    || (IndexMap::new(), IndexMap::new()),
                    |m| Self::build_imported_type_sources(m, callee_module),
                );
                let callee_newtypes = Self::build_module_map(
                    &self.all_newtypes,
                    callee_module,
                    &callee_imported,
                    &callee_original_names,
                );
                let callee_struct_fields = Self::build_module_map(
                    &self.all_struct_fields,
                    callee_module,
                    &callee_imported,
                    &callee_original_names,
                );
                let callee_variant_cases = Self::build_module_map(
                    &self.all_variant_cases,
                    callee_module,
                    &callee_imported,
                    &callee_original_names,
                );
                let callee_enum_cases = Self::build_module_map(
                    &self.all_enum_cases,
                    callee_module,
                    &callee_imported,
                    &callee_original_names,
                );
                let callee_resource_types = Self::build_module_map(
                    &self.all_resource_types,
                    callee_module,
                    &callee_imported,
                    &callee_original_names,
                );

                // Temporarily swap in callee's flat maps
                let old_newtypes = std::mem::replace(&mut self.newtypes, callee_newtypes);
                let old_struct_fields =
                    std::mem::replace(&mut self.struct_fields, callee_struct_fields);
                let old_variant_cases =
                    std::mem::replace(&mut self.variant_cases, callee_variant_cases);
                let old_enum_cases = std::mem::replace(&mut self.enum_cases, callee_enum_cases);
                let old_resource_types =
                    std::mem::replace(&mut self.resource_types, callee_resource_types);

                let resolved = self.resolve_type(&return_type_ast);

                // Restore caller's flat maps and type params
                self.newtypes = old_newtypes;
                self.struct_fields = old_struct_fields;
                self.variant_cases = old_variant_cases;
                self.enum_cases = old_enum_cases;
                self.resource_types = old_resource_types;
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
                    // First check if it's a registered newtype in newtypes
                    if let Some(&newtype_id) = self.newtypes.get(&named.name) {
                        return newtype_id;
                    }
                    // Otherwise, try to resolve via WASI registry's newtypes
                    let aliased = self.wasi_registry.get_newtype(&named.name).cloned();
                    if let Some(aliased) = aliased {
                        // Create a newtype for this WASI newtype
                        let base_type = self.resolve_wasi_type(&aliased);
                        let newtype_id = self.type_table.borrow_mut().make_newtype(
                            named.name.clone(),
                            ModuleSource::wasi("clocks"),
                            base_type,
                        );
                        // Cache the newtype for future lookups
                        self.newtypes.insert(named.name.clone(), newtype_id);
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

    /// Get the String struct type (from core:prelude/string.wado)
    fn get_string_struct_type(&mut self) -> TypeId {
        self.type_table.borrow_mut().make_struct(
            "String".to_string(),
            ModuleSource::core("prelude/string.wado"),
        )
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
                value: high.cast_unsigned(),
                repr: high.to_string(),
            },
            if type_name == "u128" {
                TypeTable::U64
            } else {
                TypeTable::I64
            },
            span,
        );

        let method_info =
            LocalMethodName::new(type_name.to_string(), None, "from_pair".to_string());
        let mangled_func_name = method_info.to_mangled_name();

        TirExpr::new(
            TirExprKind::StaticCall {
                func: FunctionRef::External {
                    module_source: ModuleSource::core("prelude/int128.wado"),
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
                    && let Some(module) = self.loaded_modules.get(&symbol.module_source)
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
                    let _ = self.logger.error(TypeError::InvalidLiteral {
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
                        let _ = self.logger.error(TypeError::InvalidLiteral {
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
                    let _ = self.logger.error(TypeError::InvalidLiteral {
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
                        let neg_value = (value as i64).wrapping_neg().cast_unsigned();
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
                        let _ = self.logger.error(TypeError::InvalidLiteral {
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
                        let _ = self.logger.error(TypeError::InvalidLiteral {
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
                        let _ = self.logger.error(TypeError::InvalidLiteral {
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
                            let method_info =
                                LocalMethodName::new(name.clone(), None, method_name.to_string());
                            let mangled_func_name = method_info.to_mangled_name();

                            return TirExpr::new(
                                TirExprKind::StaticCall {
                                    func: FunctionRef::External {
                                        module_source: ModuleSource::core("prelude/int128.wado"),
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
                        let _ = self.logger.error(TypeError::InvalidLiteral {
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
                        let _ = self.logger.error(TypeError::InvalidLiteral {
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
                    let _ = self.logger.error(TypeError::InvalidLiteral {
                        message: format!("invalid i128 literal: -{}", num_lit.repr),
                        span: unary.span,
                    });
                }
            }
        }

        // Handle null literal coercion: type null as the expected Option<T> type
        // instead of Option<Unknown>
        if let Some(target_type) = expected_type
            && let Expr::Literal(lit) = expr
            && matches!(&lit.value, Literal::Null)
            && matches!(
                self.type_table.borrow().get(target_type),
                ResolvedType::Option(_)
            )
        {
            return TirExpr::new(TirExprKind::Null, target_type, lit.span);
        }

        // Handle string literal coercion to String newtypes (e.g., "foo" → FieldName)
        if let Some(target_type) = expected_type
            && let Expr::Literal(lit) = expr
            && matches!(&lit.value, Literal::String(_))
        {
            let base_id = self.type_table.borrow().get_ultimate_base_type(target_type);
            let is_string_newtype = matches!(
                self.type_table.borrow().get(base_id),
                ResolvedType::Struct { name, .. } if name == "String"
            ) && target_type != base_id;
            if is_string_newtype {
                let mut resolved = self.resolve_expr(expr, ctx);
                resolved.type_id = target_type;
                return resolved;
            }
        }

        // Handle template string coercion to String newtypes
        if let Some(target_type) = expected_type
            && let Expr::TemplateString(_) = expr
        {
            let base_id = self.type_table.borrow().get_ultimate_base_type(target_type);
            let is_string_newtype = matches!(
                self.type_table.borrow().get(base_id),
                ResolvedType::Struct { name, .. } if name == "String"
            ) && target_type != base_id;
            if is_string_newtype {
                let mut resolved = self.resolve_expr(expr, ctx);
                resolved.type_id = target_type;
                return resolved;
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

        // Handle if expression coercion - propagate expected type to then/else branches
        if let Some(target_type) = expected_type
            && let Expr::If(if_expr) = expr
        {
            return self.resolve_if_expr_with_expected_type(if_expr, ctx, target_type);
        }

        // Handle binary expression with two numeric literals:
        // propagate expected type so both operands coerce correctly
        // e.g., `carry = 0 - 1` where carry is i64
        if let Some(target_type) = expected_type
            && let Expr::Binary(binary) = expr
            && self.type_table.borrow().is_numeric(target_type)
            && self.is_numeric_literal(&binary.left)
            && self.is_numeric_literal(&binary.right)
        {
            return self.resolve_binary_with_expected_type(binary, ctx, Some(target_type));
        }

        // Normal expression resolution
        self.resolve_expr(expr, ctx)
    }

    /// Resolve a type without registering new types
    /// This is used for lookups where we need immutable access. It only handles
    /// primitive types and newtypes. For generic types, use `resolve_type` instead.
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
        // NOTE: args are resolved later (after method lookup) to enable literal coercion
        // using the method's parameter types as expected types.

        // Resolve explicit type arguments (method-level type args)
        let type_args: Vec<TypeId> = method_call
            .type_args
            .iter()
            .map(|ty| self.resolve_type(ty))
            .collect();

        // Get the base (non-ref) type for method lookup and struct name extraction
        let base_type_id = self.get_base_type(receiver.type_id);

        // Get struct name and module source from base type
        // The struct_module is where the struct is defined (and inherent methods live)
        let (struct_name, struct_module) = match self.type_table.borrow().get(base_type_id) {
            ResolvedType::Struct {
                name,
                module_source,
                ..
            } => (name.clone(), module_source.clone()),
            ResolvedType::GenericInstance {
                name,
                module_source,
                ..
            } => (name.clone(), module_source.clone()),
            // Primitive types have impl blocks in core:prelude/primitives
            ResolvedType::Primitive(_) => {
                let prim_module = ModuleSource::Core {
                    name: "prelude/primitives.wado".to_string(),
                };
                (
                    self.type_table.borrow().mangle_type_name(base_type_id),
                    prim_module,
                )
            }
            // BuiltinArray is Array - impl blocks are in core:prelude/array.wado
            ResolvedType::BuiltinArray(_) => {
                let array_module = ModuleSource::Core {
                    name: "prelude/array.wado".to_string(),
                };
                ("Array".to_string(), array_module)
            }
            // Enum types - use enum name and its defining module
            ResolvedType::Enum {
                name,
                module_source,
            } => (name.clone(), module_source.clone()),
            _ => (
                self.type_table.borrow().mangle_type_name(base_type_id),
                self.current_module_source.clone(),
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
                &struct_module,
                receiver_type_args_for_trait.as_deref(),
            )
        {
            trait_name = Some(found_trait);
            method_info = Some(info);
            trait_impl_module_source = Some(impl_source);
        }

        // If still not found and receiver is a TypeParam, try trait bounds
        // e.g., T: Ord -> look up cmp() in Ord trait declaration
        if method_info.is_none() {
            let type_param_name = {
                let resolved = self.type_table.borrow().get(base_type_id).clone();
                if let ResolvedType::TypeParam { name, .. } = resolved {
                    Some(name)
                } else {
                    None
                }
            };
            if let Some(name) = type_param_name
                && let Some(bounds) = self.current_type_param_bounds.get(&name).cloned()
                && let Some((found_trait, info)) =
                    self.find_method_in_trait_bounds(&bounds, &method_call.method, base_type_id)
            {
                trait_name = Some(found_trait);
                method_info = Some(info);
            }
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
            let _ = self.logger.error(TypeError::TypeMismatch {
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

        // Resolve arguments with coercion using method parameter types
        let args: Vec<TirExpr> = method_call
            .args
            .iter()
            .enumerate()
            .map(|(i, arg)| {
                let expected_type = expected_param_types.get(i).copied();
                self.resolve_expr_with_expected_type(arg, ctx, expected_type)
            })
            .collect();

        // Check each argument against expected parameter type
        for (i, (arg, &expected_type)) in args.iter().zip(expected_param_types.iter()).enumerate() {
            if let Some((expected_name, actual_name)) =
                self.check_newtype_arg_mismatch(arg.type_id, expected_type)
            {
                let _ = self.logger.error(TypeError::TypeMismatch {
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

        let mangled_method_name = MethodName::format_local(
            &receiver_struct_name,
            trait_name.as_deref(),
            &method_call.method,
        );

        // Build monomorph_info for method calls on generic types
        let monomorph_info = receiver_type_args.map(|type_args| {
            let generic_name =
                MethodName::format_local(&base_struct_name, None, &method_call.method);
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
        let is_type_param_receiver = matches!(
            self.type_table.borrow().get(base_type_id),
            ResolvedType::TypeParam { .. }
        );
        let mut method_info = LocalMethodName::new(
            base_struct_name, // Use base struct name without type params
            trait_name,
            method_call.method.clone(),
        )
        .with_type_args(&impl_type_arg_names, &method_type_arg_names);
        method_info.is_type_param_receiver = is_type_param_receiver;

        // Use trait impl module source if this is a trait method,
        // otherwise use the struct's module (where inherent methods are defined)
        let method_module_source = trait_impl_module_source
            .or(Some(struct_module.clone()))
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
                        let _ = self.logger.error(TypeError::ArgumentCountMismatch {
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
                        let _ = self.logger.error(TypeError::ArgumentCountMismatch {
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

        // Handle Future::<T>::new() and Stream::<T>::new()
        // Creates a handle pair [Future<T>/Stream<T>, i32] (rx, tx)
        {
            let target_resolved = self.type_table.borrow().get(target_type_id).clone();
            let pair_info = match &target_resolved {
                ResolvedType::Future(_) if static_call.method == "new" && args.is_empty() => {
                    Some(("future_create_pair", target_type_id))
                }
                ResolvedType::Stream(_) if static_call.method == "new" && args.is_empty() => {
                    Some(("stream_create_pair", target_type_id))
                }
                _ => None,
            };

            if let Some((builtin_name, handle_type)) = pair_info {
                let i32_type = self
                    .type_table
                    .borrow_mut()
                    .intern(ResolvedType::Primitive(PrimitiveType::I32));
                let tuple_type = self
                    .type_table
                    .borrow_mut()
                    .intern(ResolvedType::Tuple(vec![handle_type, i32_type]));

                return TirExpr::new(
                    TirExprKind::Call {
                        func: FunctionRef::External {
                            module_source: ModuleSource::core("builtin"),
                            name: builtin_name.to_string(),
                            monomorph_info: None,
                            method_info: None,
                        },
                        type_args: vec![],
                        args: vec![],
                    },
                    tuple_type,
                    static_call.span,
                );
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
                        let _ = self.logger.error(TypeError::ArgumentCountMismatch {
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
                    let _ = self.logger.error(TypeError::UnknownFunction {
                        name: format!("{}::{}", name, static_call.method),
                        span: static_call.span,
                    });
                    return TirExpr::new(TirExprKind::Unit, TypeTable::ERROR, static_call.span);
                }
            } else {
                // Variant not found in variant_cases (shouldn't happen)
                let _ = self.logger.error(TypeError::UnknownType {
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
                        let _ = self.logger.error(TypeError::ArgumentCountMismatch {
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
                    let _ = self.logger.error(TypeError::UnknownFunction {
                        name: format!("{}::{}", name, static_call.method),
                        span: static_call.span,
                    });
                    return TirExpr::new(TirExprKind::Unit, TypeTable::ERROR, static_call.span);
                }
            }
        }

        let (struct_name, struct_module, mangled_struct_name, struct_type_args) = match self
            .type_table
            .borrow()
            .get(target_type_id)
        {
            ResolvedType::Struct {
                name,
                module_source,
                ..
            } => (name.clone(), module_source.clone(), name.clone(), vec![]),
            ResolvedType::Resource {
                name,
                module_source,
            } => (name.clone(), module_source.clone(), name.clone(), vec![]),
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
                    module_source.clone(),
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
                    } => (name.clone(), module_source.clone(), name.clone(), vec![]),
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
                            module_source.clone(),
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
                                        module_source.clone(),
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

        let mangled_func_name =
            MethodName::format_local(&mangled_struct_name, None, &static_call.method);

        // Look up return type
        let mut return_type = self.lookup_static_method_return_type(
            &struct_name,
            &struct_module,
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
                let generic_name =
                    MethodName::format_local(&struct_name, None, &static_call.method);
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
                    module_source: struct_module,
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
        struct_module: &ModuleSource,
        method_name: &str,
        mangled_func_name: &str,
    ) -> TypeId {
        // First check locally registered function_return_types
        if let Some(&return_type) = self.function_return_types.get(mangled_func_name) {
            return return_type;
        }

        // Also try with just StructName::method (for non-generic types)
        let simple_name = MethodName::format_local(struct_name, None, method_name);
        if let Some(&return_type) = self.function_return_types.get(&simple_name) {
            return return_type;
        }

        // Try looking up in loaded modules
        if !struct_module.is_entry_point()
            && let Some(module) = self.loaded_modules.get(struct_module)
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
                            // Set up type parameters from resource declaration before resolving
                            let old_type_params = std::mem::take(&mut self.current_type_params);

                            for (i, param) in resource.type_params.iter().enumerate() {
                                let name = &param.name;
                                if !self.current_type_params.contains_key(name) {
                                    let type_id = self
                                        .type_table
                                        .borrow_mut()
                                        .make_type_param(name.clone(), i as u32);
                                    self.current_type_params
                                        .insert(name.clone(), (i as u32, type_id));
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

        // Search all loaded modules if struct_module is entry point
        if struct_module.is_entry_point() {
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
                            // Set up type parameters from resource declaration before resolving
                            let old_type_params = std::mem::take(&mut self.current_type_params);

                            for (i, param) in resource.type_params.iter().enumerate() {
                                let name = &param.name;
                                if !self.current_type_params.contains_key(name) {
                                    let type_id = self
                                        .type_table
                                        .borrow_mut()
                                        .make_type_param(name.clone(), i as u32);
                                    self.current_type_params
                                        .insert(name.clone(), (i as u32, type_id));
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
        match expr {
            Expr::Literal(lit) => matches!(lit.value, Literal::Number(_)),
            Expr::Unary(unary) if unary.op == UnaryOp::Neg => {
                matches!(&unary.expr, Expr::Literal(lit) if matches!(lit.value, Literal::Number(_)))
            }
            _ => false,
        }
    }

    /// Check if a qualified name `struct_name::method_name` is a static method
    fn is_static_method(&self, struct_name: &str, method_name: &str) -> bool {
        let mangled_name = MethodName::format_local(struct_name, None, method_name);

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
        if let Some(&newtype_id) = self.newtypes.get(struct_name)
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
            if let Some(&newtype_id) = self.newtypes.get(struct_name) {
                if let ResolvedType::Newtype { base_type, .. } =
                    self.type_table.borrow().get(newtype_id).clone()
                {
                    // Follow the chain to find the ultimate struct
                    let base_name = self.get_ultimate_base_struct_name(base_type);
                    let mangled = MethodName::format_local(&base_name, None, method_name);
                    (base_name, mangled)
                } else {
                    (struct_name.to_string(), mangled_func_name.to_string())
                }
            } else {
                (struct_name.to_string(), mangled_func_name.to_string())
            };

        // Determine module source for the actual struct
        let struct_module = self.find_struct_module_source(&actual_struct_name);

        // Look up return type using the actual struct name
        let return_type = self.lookup_static_method_return_type(
            &actual_struct_name,
            &struct_module,
            method_name,
            &actual_mangled_name,
        );

        TirExpr::new(
            TirExprKind::StaticCall {
                func: FunctionRef::External {
                    module_source: struct_module,
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
        // Check if it's a primitive type - impl blocks live in core:prelude/primitives.wado
        // Note: i128/u128 are structs (in prelude/int128.wado), not primitives
        if matches!(
            struct_name,
            "i8" | "i16"
                | "i32"
                | "i64"
                | "u8"
                | "u16"
                | "u32"
                | "u64"
                | "f32"
                | "f64"
                | "bool"
                | "char"
        ) {
            return ModuleSource::Core {
                name: "prelude/primitives.wado".to_string(),
            };
        }

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
        for (module_source, module) in self.loaded_modules {
            for item in &module.items {
                match item {
                    Item::Struct(s) if s.name == struct_name => {
                        return module_source.clone();
                    }
                    Item::Resource(r) if r.name == struct_name => {
                        return module_source.clone();
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

        // Get the struct name, module source, and type args from the base type
        // For primitives, module_source is None to trigger "search all loaded modules" logic
        let (struct_name, struct_module_source, receiver_type_args, newtype_base) = match &base_type
        {
            ResolvedType::Struct {
                name,
                module_source,
                ..
            } => (name.clone(), Some(module_source.clone()), None, None),
            // Resource types use reference semantics - handle like struct for method lookup
            ResolvedType::Resource {
                name,
                module_source,
            } => (name.clone(), Some(module_source.clone()), None, None),
            // Generic instances like Box<i32> use the base name "Box" for method lookup
            ResolvedType::GenericInstance {
                name,
                module_source,
                type_args,
            } => (
                name.clone(),
                Some(module_source.clone()),
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
                Some(module_source.clone()),
                None,
                Some(*base_type),
            ),
            // Primitive types - search for impl blocks in loaded modules
            // (e.g., impl i32 { fn to_string(&self) -> String { ... } })
            ResolvedType::Primitive(prim) => {
                // Use None to trigger "search all loaded modules" logic
                (prim.as_str().to_string(), None, None, None)
            }
            // Enum types - search for impl blocks by enum name
            ResolvedType::Enum {
                name,
                module_source,
            } => (name.clone(), Some(module_source.clone()), None, None),
            _ => return None,
        };

        let mangled_name = MethodName::format_local(&struct_name, None, method_name);
        if let Some(&return_type) = self.function_return_types.get(&mangled_name) {
            // For locally registered methods, find self_kind and param_types from the AST
            // Also checks that bounded impl block constraints are satisfied
            if let Some((self_kind, param_types)) = self.find_local_method_info(
                &struct_name,
                method_name,
                receiver_type_args.as_deref(),
            ) {
                return Some(MethodInfo {
                    return_type,
                    self_kind,
                    param_types,
                    inherited_from_base: None,
                });
            }
            // If find_local_method_info returned None, the method either doesn't exist
            // or its impl block bounds are not satisfied. Don't fall back - continue
            // searching loaded modules and trait methods.
        }

        // Try looking up in loaded modules (for imported structs)
        // Only check inherent impls (not trait impls) - trait impls are handled separately
        if let Some(ref module_source) = struct_module_source {
            // Pre-populate module type maps cache before borrowing loaded_modules
            self.ensure_module_maps_cached(module_source);
            if let Some(module) = self.loaded_modules.get(module_source) {
                for item in &module.items {
                    if let Item::Impl(impl_block) = item {
                        // Skip trait impls - only look at inherent impls
                        if impl_block.trait_type.is_some() {
                            continue;
                        }
                        let impl_struct_name = self.get_type_name(&impl_block.ty);
                        if impl_struct_name == struct_name
                            && self
                                .check_impl_block_bounds(impl_block, receiver_type_args.as_deref())
                        {
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

                                    // Resolve return type and param types in the source module's
                                    // type context, not the caller's. This prevents same-named types
                                    // from different modules being confused (e.g., both modules
                                    // define "Config" with different fields).
                                    // Use cached module type maps (O(1) swap) instead of
                                    // rebuilding maps from scratch on every call.
                                    let mut cached = self
                                        .module_type_maps_cache
                                        .shift_remove(module_source)
                                        .expect("cache populated by ensure_module_maps_cached");
                                    std::mem::swap(
                                        &mut self.struct_fields,
                                        &mut cached.struct_fields,
                                    );
                                    std::mem::swap(
                                        &mut self.variant_cases,
                                        &mut cached.variant_cases,
                                    );
                                    std::mem::swap(&mut self.enum_cases, &mut cached.enum_cases);
                                    std::mem::swap(&mut self.newtypes, &mut cached.newtypes);
                                    std::mem::swap(
                                        &mut self.resource_types,
                                        &mut cached.resource_types,
                                    );

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

                                    std::mem::swap(
                                        &mut self.struct_fields,
                                        &mut cached.struct_fields,
                                    );
                                    std::mem::swap(
                                        &mut self.variant_cases,
                                        &mut cached.variant_cases,
                                    );
                                    std::mem::swap(&mut self.enum_cases, &mut cached.enum_cases);
                                    std::mem::swap(&mut self.newtypes, &mut cached.newtypes);
                                    std::mem::swap(
                                        &mut self.resource_types,
                                        &mut cached.resource_types,
                                    );
                                    self.module_type_maps_cache
                                        .insert(module_source.clone(), cached);
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

        // Search all loaded modules if no specific module (for prelude types)
        // Only check inherent impls (not trait impls) - trait impls are handled separately
        if struct_module_source.is_none() {
            for module in self.loaded_modules.values() {
                for item in &module.items {
                    if let Item::Impl(impl_block) = item {
                        // Skip trait impls - only look at inherent impls
                        if impl_block.trait_type.is_some() {
                            continue;
                        }
                        let impl_struct_name = self.get_type_name(&impl_block.ty);
                        if impl_struct_name == struct_name
                            && self
                                .check_impl_block_bounds(impl_block, receiver_type_args.as_deref())
                        {
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
        if let Some(ref module_source) = struct_module_source
            && let Some(module) = self.loaded_modules.get(module_source)
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

        // Also search all modules for resources if no specific module
        if struct_module_source.is_none() {
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
        receiver_type_args: Option<&[TypeId]>,
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
                if impl_struct_name == struct_name
                    && self.check_impl_block_bounds(impl_block, receiver_type_args)
                {
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

        let (struct_name, struct_module_source) = match &base_type {
            ResolvedType::Struct {
                name,
                module_source,
                ..
            } => (name.clone(), Some(module_source.clone())),
            ResolvedType::GenericInstance {
                name,
                module_source,
                ..
            } => (name.clone(), Some(module_source.clone())),
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
        if let Some(ref module_source) = struct_module_source
            && let Some(module) = self.loaded_modules.get(module_source)
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
        struct_module: &ModuleSource,
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
        if !struct_module.is_entry_point()
            && let Some(module) = self.loaded_modules.get(struct_module)
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
                        struct_module.clone(),
                    ));
                }
            }
        }

        // Also check all loaded modules
        for (module_src, module) in self.loaded_modules {
            for item in &module.items {
                if let Item::Impl(impl_block) = item
                    && let Some(trait_type) = &impl_block.trait_type
                {
                    impl_blocks_to_check.push((
                        impl_block.ty.clone(),
                        trait_type.clone(),
                        impl_block.methods.clone(),
                        impl_block.associated_types.clone(),
                        module_src.clone(),
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
            if let Some(&newtype_id) = self.newtypes.get(struct_name) {
                let mut current = newtype_id;
                while let ResolvedType::Newtype { base_type, .. } =
                    self.type_table.borrow().get(current).clone()
                {
                    let base_name = self.type_table.borrow().type_name(base_type);
                    names.push(base_name);
                    current = base_type;
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

                let mut method_found = false;
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
                        method_found = true;
                    }
                }

                // If the method wasn't found in the impl block, check the trait
                // declaration for a default method with that name
                if !method_found {
                    let trait_name_str = self.get_type_name(&trait_type);
                    if let Some(trait_methods) = self.find_trait_decl_methods(&trait_name_str) {
                        for default_method in &trait_methods {
                            if default_method.name == method_name && default_method.body.is_some() {
                                let return_type = default_method
                                    .return_type
                                    .as_ref()
                                    .map(|t| self.resolve_type(t))
                                    .unwrap_or(TypeTable::UNIT);
                                let self_kind = default_method
                                    .params
                                    .first()
                                    .map(|p| p.self_kind)
                                    .unwrap_or(ast::SelfKind::None);
                                let param_types = self.extract_param_types(&default_method.params);
                                found_traits.push((
                                    trait_name_str.clone(),
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

    /// Find a trait declaration by name across all modules.
    /// Returns the trait's methods (cloned) if found.
    fn find_trait_decl_methods(&self, trait_name: &str) -> Option<Vec<ast::Function>> {
        for module in self.loaded_modules.values() {
            for item in &module.items {
                if let Item::Trait(trait_decl) = item
                    && trait_decl.name == trait_name
                {
                    return Some(trait_decl.methods.clone());
                }
            }
        }
        for item in &self.current_module_items {
            if let Item::Trait(trait_decl) = item
                && trait_decl.name == trait_name
            {
                return Some(trait_decl.methods.clone());
            }
        }
        None
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
        self.find_comparison_trait_impl(struct_name, base_type_id, "Ord", "cmp")
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
                    let mut assoc_type_map: IndexMap<String, TypeId> = IndexMap::new();

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

        // All enums automatically implement Eq, Ord, and Display
        if let ResolvedType::Enum { .. } = &resolved {
            match trait_name {
                "Eq" | "Ord" | "Display" => return true,
                _ => {}
            }
        }

        // Get the type name and type args for looking up implementations
        let (type_name, type_args) = match &resolved {
            ResolvedType::Struct { name, .. } => (name.clone(), None),
            ResolvedType::GenericInstance {
                name, type_args, ..
            } => (
                name.clone(),
                if type_args.is_empty() {
                    None
                } else {
                    Some(type_args.clone())
                },
            ),
            ResolvedType::BuiltinArray(elem) => ("Array".to_string(), Some(vec![*elem])),
            ResolvedType::Option(_) => ("Option".to_string(), None),
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
                // For references, check if the inner type implements the trait
                return self.type_implements_trait(*inner, trait_name);
            }
            _ => return false,
        };

        self.find_trait_impl_for_type_with_args(&type_name, trait_name, type_args.as_deref())
    }

    /// Helper to check if there's an impl block for a type implementing a trait
    fn find_trait_impl_for_type(&self, type_name: &str, trait_name: &str) -> bool {
        self.find_trait_impl_for_type_with_args(type_name, trait_name, None)
    }

    /// Check if there's a trait impl for a type, with optional type args for bounds checking.
    /// For `impl<T: Eq> Eq for Array<T>`, when checking `Array<Foo>`, passes `[Foo]` as `type_args`.
    fn find_trait_impl_for_type_with_args(
        &self,
        type_name: &str,
        trait_name: &str,
        type_args: Option<&[TypeId]>,
    ) -> bool {
        // Check all loaded modules
        for module in self.loaded_modules.values() {
            for item in &module.items {
                if let Item::Impl(impl_block) = item
                    && let Some(trait_type) = &impl_block.trait_type
                {
                    let impl_type_name = self.get_type_name(&impl_block.ty);
                    let impl_trait_name = self.get_type_name(trait_type);

                    if impl_type_name == type_name
                        && impl_trait_name == trait_name
                        && self.check_impl_block_bounds(impl_block, type_args)
                    {
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

                if impl_type_name == type_name
                    && impl_trait_name == trait_name
                    && self.check_impl_block_bounds(impl_block, type_args)
                {
                    return true;
                }
            }
        }

        false
    }

    /// Find a method in the trait declarations given by the bound names.
    /// For example, if T: Ord, look up the "cmp" method in the Ord trait declaration.
    /// Returns (`trait_name`, `MethodInfo`) with the method's return type, `self_kind`, and `param_types`,
    /// where Self is substituted with the `TypeParam`'s type.
    fn find_method_in_trait_bounds(
        &mut self,
        bounds: &[String],
        method_name: &str,
        self_type_id: TypeId,
    ) -> Option<(String, MethodInfo)> {
        // Collect trait declarations from all modules
        for trait_name in bounds {
            // Search all loaded modules for the trait declaration
            let mut found_trait_method: Option<(ast::Function, ModuleSource)> = None;

            for (module_src, module) in self.loaded_modules {
                for item in &module.items {
                    if let Item::Trait(trait_decl) = item
                        && trait_decl.name == *trait_name
                    {
                        for method in &trait_decl.methods {
                            if method.name == method_name {
                                found_trait_method = Some((method.clone(), module_src.clone()));
                                break;
                            }
                        }
                    }
                }
                if found_trait_method.is_some() {
                    break;
                }
            }

            // Also check current module items
            if found_trait_method.is_none() {
                for item in &self.current_module_items {
                    if let Item::Trait(trait_decl) = item
                        && trait_decl.name == *trait_name
                    {
                        for method in &trait_decl.methods {
                            if method.name == method_name {
                                found_trait_method =
                                    Some((method.clone(), self.current_module_source.clone()));
                                break;
                            }
                        }
                    }
                }
            }

            if let Some((method, _module_source)) = found_trait_method {
                // Resolve the method signature with Self = self_type_id (the TypeParam)
                let old_self_type = self.current_self_type;
                self.current_self_type = Some(self_type_id);

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

                self.current_self_type = old_self_type;

                return Some((
                    trait_name.clone(),
                    MethodInfo {
                        return_type,
                        self_kind,
                        param_types,
                        inherited_from_base: None,
                    },
                ));
            }
        }

        None
    }

    /// Check if an impl block's type parameter bounds are satisfied by the given type args.
    /// For `impl<T: Ord> Array<T>`, checks that the concrete type substituted for T implements Ord.
    fn check_impl_block_bounds(
        &self,
        impl_block: &ast::ImplBlock,
        type_args: Option<&[TypeId]>,
    ) -> bool {
        // No type params with bounds → always OK
        if impl_block.type_params.iter().all(|p| p.bounds.is_empty()) {
            return true;
        }

        let Some(type_args) = type_args else {
            // No type args to check (non-generic receiver) → skip bounds check
            return true;
        };

        // Build name → bounds map from impl block type params
        let bounds_map: IndexMap<&str, &[String]> = impl_block
            .type_params
            .iter()
            .filter(|p| !p.bounds.is_empty())
            .map(|p| (p.name.as_str(), p.bounds.as_slice()))
            .collect();

        // Match type params to receiver type args via generic type arg positions
        if let ast::Type::Generic(generic) = &impl_block.ty {
            for (i, arg) in generic.args.iter().enumerate() {
                if let ast::Type::Named(named) = arg
                    && let Some(bounds) = bounds_map.get(named.name.as_str())
                    && let Some(&type_arg) = type_args.get(i)
                {
                    // If the type arg is itself a type parameter (e.g., T in a generic context),
                    // skip the bounds check. Within a bounded impl block, type params are assumed
                    // to satisfy bounds; concrete types are checked at call sites.
                    if matches!(
                        self.type_table.borrow().get(type_arg),
                        ResolvedType::TypeParam { .. }
                    ) {
                        continue;
                    }
                    for bound in *bounds {
                        if !self.type_implements_trait(type_arg, bound) {
                            return false;
                        }
                    }
                }
            }
        }

        true
    }

    /// Check trait bounds on a generic function's type arguments.
    /// Looks up the function's type params and validates bounds against the provided type args.
    fn check_function_type_arg_bounds(
        &mut self,
        callee_module: &ModuleSource,
        func_name: &str,
        type_args: &[TypeId],
        span: Span,
    ) {
        // Look up function's type params from AST
        let type_params = self.lookup_function_type_params(callee_module, func_name);
        for (i, param) in type_params.iter().enumerate() {
            if let Some(&type_arg) = type_args.get(i) {
                for bound in &param.bounds {
                    if !self.type_implements_trait(type_arg, bound) {
                        let type_name = self.type_id_to_string(type_arg);
                        let _ = self.logger.error(TypeError::TraitBoundNotSatisfied {
                            type_name,
                            trait_name: bound.clone(),
                            param_name: param.name.clone(),
                            span,
                        });
                    }
                }
            }
        }
    }

    /// Look up the type parameters of a function from its AST definition.
    fn lookup_function_type_params(
        &self,
        callee_module: &ModuleSource,
        func_name: &str,
    ) -> Vec<ast::GenericParam> {
        // Try local functions
        if callee_module.is_entry_point() {
            for item in &self.current_module_items {
                if let ast::Item::Function(func) = item
                    && func.name == func_name
                {
                    return func.type_params.clone();
                }
            }
        }

        // Try loaded modules
        if let Some(module) = self.loaded_modules.get(callee_module) {
            for item in &module.items {
                if let ast::Item::Function(func) = item
                    && func.name == func_name
                {
                    return func.type_params.clone();
                }
            }
        }

        Vec::new()
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
    ) -> IndexMap<String, TypeId> {
        let mut mapping = IndexMap::new();

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
        type_param_mapping: &IndexMap<String, TypeId>,
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

                // Special-case Option to use its dedicated type
                let base_name = &g.name;
                if base_name == "Option" {
                    let inner = resolved_args.first().copied().unwrap_or(TypeTable::UNKNOWN);
                    self.type_table.borrow_mut().make_option(inner)
                } else {
                    // For generic types, create a generic instance
                    // Use the variant's defining module source if available
                    let module_source = self
                        .variant_cases
                        .get(base_name.as_str())
                        .map(|info| info.module_source.clone())
                        .unwrap_or_else(|| self.current_module_source.clone());
                    self.type_table
                        .borrow_mut()
                        .intern(ResolvedType::GenericInstance {
                            name: base_name.clone(),
                            module_source,
                            type_args: resolved_args,
                        })
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
        let struct_name = match self.type_table.borrow().get(base_type_id).clone() {
            ResolvedType::Struct { name, .. } => name,
            ResolvedType::GenericInstance { name, .. } => name,
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

        let (output_struct_name, output_module_source, output_type_args) =
            match self.type_table.borrow().get(output_base_type_id).clone() {
                ResolvedType::Struct {
                    name,
                    module_source,
                    ..
                } => (name, module_source, None),
                ResolvedType::GenericInstance {
                    name,
                    module_source,
                    type_args,
                } => (
                    name,
                    module_source,
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
                    self.current_module_source.clone(),
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
                &output_module_source,
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
            MethodName::format_local(&struct_name, Some(&index_mut_info.trait_name), "index_mut");

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

        let mangled_method_name = MethodName::format_local(
            &output_struct_name,
            method_trait_name.as_deref(),
            &method_call.method,
        );

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
        span: Span,
    ) -> (u32, TypeId) {
        // Clone the type to avoid borrow issues
        let resolved = self.type_table.borrow().get(struct_type).clone();
        match resolved {
            // Struct field access
            ResolvedType::Struct {
                name,
                module_source,
                ..
            } => {
                // Use the flat struct_fields map first. When there's a name collision
                // (the entry is from a different module), re-resolve fields from the
                // source module to get the correct struct definition.
                if let Some(struct_info) = self.struct_fields.get(&name) {
                    if struct_info.module_source == module_source {
                        for (index, (fname, ftype)) in struct_info.fields.iter().enumerate() {
                            if fname == field_name {
                                return (index as u32, *ftype);
                            }
                        }
                    } else {
                        // Name collision: re-resolve field type from the loaded module
                        if let Some(ftype) =
                            self.resolve_field_in_source_module(&name, field_name, &module_source)
                        {
                            return ftype;
                        }
                    }
                }
            }
            // Tuple field access (numeric field names: 0, 1, 2, ...)
            ResolvedType::Tuple(elements) => {
                if let Ok(index) = field_name.parse::<usize>() {
                    if index < elements.len() {
                        return (index as u32, elements[index]);
                    }
                    let _ = self.logger.error(TypeError::InvalidLiteral {
                        message: format!(
                            "tuple index {} out of bounds, tuple has {} elements",
                            index,
                            elements.len()
                        ),
                        span,
                    });
                    return (0, TypeTable::UNKNOWN);
                }
            }
            // Reference types - look through to inner type
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
                return self.lookup_field_type(inner, field_name, span);
            }
            // Newtype - look through to base type for field access
            ResolvedType::Newtype { base_type, .. } => {
                return self.lookup_field_type(base_type, field_name, span);
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

    /// Resolve a struct field type from the source module where the struct is defined.
    /// Used when the flat `struct_fields` map has a name collision (two modules define
    /// a struct with the same name). This re-resolves the field type in the source
    /// module's type context to get the correct result.
    fn resolve_field_in_source_module(
        &mut self,
        struct_name: &str,
        field_name: &str,
        module_source: &ModuleSource,
    ) -> Option<(u32, TypeId)> {
        // Pre-populate module type maps cache before borrowing loaded_modules
        self.ensure_module_maps_cached(module_source);

        let loaded_module = self.loaded_modules.get(module_source)?;
        // Find the struct declaration in the loaded module
        let struct_decl = loaded_module.items.iter().find_map(|item| {
            if let Item::Struct(s) = item
                && s.name == struct_name
            {
                Some(s)
            } else {
                None
            }
        })?;

        // Swap to source module's type context using cached maps (O(1))
        let mut cached = self
            .module_type_maps_cache
            .shift_remove(module_source)
            .expect("cache populated by ensure_module_maps_cached");
        std::mem::swap(&mut self.struct_fields, &mut cached.struct_fields);
        std::mem::swap(&mut self.variant_cases, &mut cached.variant_cases);
        std::mem::swap(&mut self.enum_cases, &mut cached.enum_cases);
        std::mem::swap(&mut self.newtypes, &mut cached.newtypes);
        std::mem::swap(&mut self.resource_types, &mut cached.resource_types);

        // Find and resolve the field type
        let result = struct_decl
            .fields
            .iter()
            .enumerate()
            .find(|(_, f)| f.name == field_name)
            .map(|(index, f)| (index as u32, self.resolve_type(&f.ty)));

        // Restore type maps and re-cache
        std::mem::swap(&mut self.struct_fields, &mut cached.struct_fields);
        std::mem::swap(&mut self.variant_cases, &mut cached.variant_cases);
        std::mem::swap(&mut self.enum_cases, &mut cached.enum_cases);
        std::mem::swap(&mut self.newtypes, &mut cached.newtypes);
        std::mem::swap(&mut self.resource_types, &mut cached.resource_types);
        self.module_type_maps_cache
            .insert(module_source.clone(), cached);

        result
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
                    let _ = self.logger.error(TypeError::InvalidLiteral {
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
            let _ = self.logger.error(TypeError::InvalidLiteral {
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

                let mangled_method_name =
                    MethodName::format_local(&struct_name, Some(&trait_info.trait_name), "index");

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

                let mangled_method_name = MethodName::format_local(
                    &struct_name,
                    Some(&trait_info.trait_name),
                    "index_value",
                );

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
        let _ = self.logger.error(TypeError::TypeMismatch {
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
                let _ = self.logger.error(TypeError::NotYetImplemented {
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
                    let _ = self.logger.error(TypeError::TypeMismatch {
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
                let _ = self.logger.error(TypeError::TypeMismatch {
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
            let _ = self.logger.error(TypeError::TypeMismatch {
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

    /// Resolve an if expression with expected type for literal coercion.
    /// Propagates the expected type into then/else block result expressions.
    fn resolve_if_expr_with_expected_type(
        &mut self,
        if_expr: &IfExpr,
        ctx: &mut FunctionContext,
        expected_type: TypeId,
    ) -> TirExpr {
        // Handle optional init binding (scoped to this if expression)
        if if_expr.init.is_some() {
            ctx.enter_scope();
        }

        if let Some(init) = &if_expr.init {
            let _init_stmt = self.resolve_let(init, ctx);
        }

        // Resolve the condition
        let condition = match &if_expr.condition {
            ast::Condition::Expr(expr) => self.resolve_expr(expr, ctx),
            ast::Condition::Pattern { span, .. } => {
                let _ = self.logger.error(TypeError::NotYetImplemented {
                    feature: "pattern matching in if expressions (use if statement instead)"
                        .to_string(),
                    span: *span,
                });
                TirExpr::new(TirExprKind::BoolLiteral(true), TypeTable::BOOL, *span)
            }
        };

        // Resolve then/else blocks with expected type for coercion
        let then_block =
            self.resolve_block_with_expected_type(&if_expr.then_block, ctx, expected_type);
        let else_block = if_expr
            .else_block
            .as_ref()
            .map(|b| self.resolve_block_with_expected_type(b, ctx, expected_type));

        let result = TirExpr::new(
            TirExprKind::If {
                condition: Box::new(condition),
                then_branch: then_block,
                else_branch: else_block,
            },
            expected_type,
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
        // Enter scope for pattern bindings (they're only visible in the arm body and guard)
        ctx.enter_scope();

        // Resolve pattern with scrutinee type information (same as if let)
        let pattern = self.resolve_if_pattern(&arm.pattern, scrutinee_type, ctx, arm.span);

        // Resolve optional guard expression
        let guard = arm.guard.as_ref().map(|g| self.resolve_expr(g, ctx));

        // Resolve arm body
        let body = self.resolve_expr(&arm.body, ctx);

        ctx.exit_scope();

        TirMatchArm {
            pattern,
            guard,
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

        // Resolve optional guard expression
        let guard = arm.guard.as_ref().map(|g| self.resolve_expr(g, ctx));

        // Resolve arm body with expected type for coercion
        let body = self.resolve_expr_with_expected_type(&arm.body, ctx, Some(expected_type));

        ctx.exit_scope();

        TirMatchArm {
            pattern,
            guard,
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

    /// Resolve a template string - desugars to labeled block with append + `Display::fmt`
    /// `Hello, {name}!` → `__tmpl: { let mut __r = ...; __r.append("Hello, "); name.fmt(&mut __f); __r.append("!"); break __tmpl: __r; }`
    fn resolve_template_string(
        &mut self,
        template: &ast::TemplateStringExpr,
        ctx: &mut FunctionContext,
    ) -> TirExpr {
        let string_type = self.get_string_struct_type();
        let span = template.span;

        // Fast paths: empty template or single literal
        let has_interpolation = template
            .parts
            .iter()
            .any(|p| matches!(p, ast::TemplatePart::Interpolation { .. }));

        if !has_interpolation {
            // All-literal template: concatenate at compile time
            let mut combined = String::new();
            for part in &template.parts {
                if let ast::TemplatePart::String(s) = part {
                    combined.push_str(s);
                }
            }
            return TirExpr::new(TirExprKind::StringLiteral(combined), string_type, span);
        }

        // Single interpolation, no literals around it, no format spec, and already String?
        if template.parts.len() == 1
            && let ast::TemplatePart::Interpolation { expr, format: None } = &template.parts[0]
        {
            let resolved = self.resolve_expr(expr, ctx);
            if resolved.type_id == string_type {
                return resolved;
            }
        }

        // --- Build a labeled block: __tmpl: { let mut __r = ...; ...; break __tmpl: __r; } ---
        let label = "__tmpl".to_string();

        // Estimate capacity: sum of literal lengths + 16 per interpolation
        let capacity_estimate: i64 = template
            .parts
            .iter()
            .map(|p| match p {
                ast::TemplatePart::String(s) => s.len() as i64,
                ast::TemplatePart::Interpolation { .. } => 16,
            })
            .sum();

        // Enter a new scope for the block
        ctx.enter_scope();

        // let mut __r = String::with_capacity(N);
        let buf_index = ctx.add_local("__r".to_string(), string_type, true);
        let with_capacity_call = TirExpr::new(
            TirExprKind::StaticCall {
                func: FunctionRef::External {
                    module_source: string_module_source(),
                    name: "String::with_capacity".to_string(),
                    monomorph_info: None,
                    method_info: Some(LocalMethodName::new(
                        "String".to_string(),
                        None,
                        "with_capacity".to_string(),
                    )),
                },
                args: vec![TirExpr::new(
                    TirExprKind::IntLiteral {
                        value: capacity_estimate as u64,
                        repr: capacity_estimate.to_string(),
                    },
                    TypeTable::I32,
                    span,
                )],
            },
            string_type,
            span,
        );
        let mut stmts = vec![TirStmt::new(
            TirStmtKind::Let {
                name: "__r".to_string(),
                local_index: buf_index,
                is_mut: true,
                is_reactive: false,
                type_id: string_type,
                value: with_capacity_call,
            },
            span,
        )];

        // Prepare Formatter type and its &mut type
        let formatter_type = self.type_table.borrow_mut().make_struct(
            "Formatter".to_string(),
            ModuleSource::core("prelude/format.wado"),
        );
        let mut_ref_formatter = self.type_table.borrow_mut().make_mut_ref(formatter_type);

        // Track whether we've created the __f local yet
        let mut fmt_local_index: Option<u32> = None;

        // Helper closures can't capture &mut self, so we build parts inline
        for part in &template.parts {
            match part {
                ast::TemplatePart::String(s) => {
                    if s.is_empty() {
                        continue;
                    }
                    // __r.append("literal")
                    let buf_ref = TirExpr::new(
                        TirExprKind::Local {
                            index: buf_index,
                            name: "__r".to_string(),
                        },
                        string_type,
                        span,
                    );
                    let append_call = TirExpr::new(
                        TirExprKind::MethodCall {
                            receiver: Box::new(buf_ref),
                            func: FunctionRef::External {
                                module_source: string_module_source(),
                                name: "String::append".to_string(),
                                monomorph_info: None,
                                method_info: Some(LocalMethodName::new(
                                    "String".to_string(),
                                    None,
                                    "append".to_string(),
                                )),
                            },
                            type_args: vec![],
                            args: vec![TirExpr::new(
                                TirExprKind::StringLiteral(s.clone()),
                                string_type,
                                span,
                            )],
                        },
                        TypeTable::UNIT,
                        span,
                    );
                    stmts.push(TirStmt::new(TirStmtKind::Expr(append_call), span));
                }
                ast::TemplatePart::Interpolation { expr, format } => {
                    let resolved = self.resolve_expr(expr, ctx);

                    // If String type with no format spec, just append directly
                    if resolved.type_id == string_type && format.is_none() {
                        let buf_ref = TirExpr::new(
                            TirExprKind::Local {
                                index: buf_index,
                                name: "__r".to_string(),
                            },
                            string_type,
                            span,
                        );
                        let append_call = TirExpr::new(
                            TirExprKind::MethodCall {
                                receiver: Box::new(buf_ref),
                                func: FunctionRef::External {
                                    module_source: string_module_source(),
                                    name: "String::append".to_string(),
                                    monomorph_info: None,
                                    method_info: Some(LocalMethodName::new(
                                        "String".to_string(),
                                        None,
                                        "append".to_string(),
                                    )),
                                },
                                type_args: vec![],
                                args: vec![resolved],
                            },
                            TypeTable::UNIT,
                            span,
                        );
                        stmts.push(TirStmt::new(TirStmtKind::Expr(append_call), span));
                        continue;
                    }

                    // Parse format spec (if any) to determine trait + Formatter fields
                    let parsed = format.as_ref().map(|f| self.parse_format_spec(&f.spec));

                    // Determine which trait's fmt to call
                    let (trait_name, _trait_type_char) = match &parsed {
                        Some(pf) => match pf.type_char {
                            Some('b') => ("Binary", Some('b')),
                            Some('o') => ("Octal", Some('o')),
                            Some('x') => ("LowerHex", Some('x')),
                            Some('X') => ("UpperHex", Some('X')),
                            Some('e') => ("LowerExp", Some('e')),
                            Some('E') => ("UpperExp", Some('E')),
                            _ => ("Display", None),
                        },
                        None => ("Display", None),
                    };

                    // Create or reassign Formatter local
                    let fmt_index = if let Some(idx) = fmt_local_index {
                        // Reassign __f = Formatter::new(&mut __r) or Formatter { ... }
                        let formatter_expr = self.build_formatter_expr(
                            buf_index,
                            string_type,
                            formatter_type,
                            &parsed,
                            span,
                        );
                        let assign = TirExpr::new(
                            TirExprKind::Assign {
                                target: Box::new(TirExpr::new(
                                    TirExprKind::Local {
                                        index: idx,
                                        name: "__f".to_string(),
                                    },
                                    formatter_type,
                                    span,
                                )),
                                value: Box::new(formatter_expr),
                            },
                            TypeTable::UNIT,
                            span,
                        );
                        stmts.push(TirStmt::new(TirStmtKind::Expr(assign), span));
                        idx
                    } else {
                        // First interpolation: let mut __f = ...
                        let idx = ctx.add_local("__f".to_string(), formatter_type, true);
                        fmt_local_index = Some(idx);
                        let formatter_expr = self.build_formatter_expr(
                            buf_index,
                            string_type,
                            formatter_type,
                            &parsed,
                            span,
                        );
                        stmts.push(TirStmt::new(
                            TirStmtKind::Let {
                                name: "__f".to_string(),
                                local_index: idx,
                                is_mut: true,
                                is_reactive: false,
                                type_id: formatter_type,
                                value: formatter_expr,
                            },
                            span,
                        ));
                        idx
                    };

                    // Check if this is a float type with precision → call
                    // fmt_f64_fixed/fmt_f32_fixed directly to avoid pulling in
                    // the fixed-point bundled code when only shortest is needed.
                    let float_fixed_func = if trait_name == "Display"
                        && parsed.as_ref().is_some_and(|pf| pf.precision.is_some())
                    {
                        let resolved_type = self.type_table.borrow().get(resolved.type_id).clone();
                        match resolved_type {
                            ResolvedType::Primitive(PrimitiveType::F64) => Some("fmt_f64_fixed"),
                            ResolvedType::Primitive(PrimitiveType::F32) => Some("fmt_f32_fixed"),
                            _ => None,
                        }
                    } else {
                        None
                    };

                    let fmt_mut_ref = TirExpr::new(
                        TirExprKind::Unary {
                            op: TirUnaryOp::MutRef,
                            expr: Box::new(TirExpr::new(
                                TirExprKind::Local {
                                    index: fmt_index,
                                    name: "__f".to_string(),
                                },
                                formatter_type,
                                span,
                            )),
                        },
                        mut_ref_formatter,
                        span,
                    );

                    if let Some(func_name) = float_fixed_func {
                        // Direct call: fmt_f64_fixed(value, precision, &mut __f)
                        let precision_value = parsed.as_ref().unwrap().precision.unwrap();
                        let precision_expr = TirExpr::new(
                            TirExprKind::IntLiteral {
                                value: precision_value as u64,
                                repr: precision_value.to_string(),
                            },
                            TypeTable::I32,
                            span,
                        );
                        let fmt_call = TirExpr::new(
                            TirExprKind::StaticCall {
                                func: FunctionRef::External {
                                    module_source: ModuleSource::core("prelude/primitives.wado"),
                                    name: func_name.to_string(),
                                    monomorph_info: None,
                                    method_info: None,
                                },
                                args: vec![resolved, precision_expr, fmt_mut_ref],
                            },
                            TypeTable::UNIT,
                            span,
                        );
                        stmts.push(TirStmt::new(TirStmtKind::Expr(fmt_call), span));
                    } else {
                        // Standard path: receiver.fmt(&mut __f)
                        let base_type_id = self.get_ultimate_base_type(resolved.type_id);
                        let (receiver_type_name, impl_module_source) = self
                            .resolve_display_impl_source(
                                base_type_id,
                                resolved.type_id,
                                trait_name,
                            );

                        let receiver_expr = {
                            let resolved_type =
                                self.type_table.borrow().get(resolved.type_id).clone();
                            match resolved_type {
                                ResolvedType::Ref(_) | ResolvedType::MutRef(_) => resolved,
                                _ => {
                                    let ref_type =
                                        self.type_table.borrow_mut().make_ref(resolved.type_id);
                                    TirExpr::new(
                                        TirExprKind::Unary {
                                            op: TirUnaryOp::Ref,
                                            expr: Box::new(resolved),
                                        },
                                        ref_type,
                                        span,
                                    )
                                }
                            }
                        };
                        let mangled_name =
                            MethodName::format_local(&receiver_type_name, Some(trait_name), "fmt");
                        let fmt_call = TirExpr::new(
                            TirExprKind::MethodCall {
                                receiver: Box::new(receiver_expr),
                                func: FunctionRef::External {
                                    module_source: impl_module_source,
                                    name: mangled_name,
                                    monomorph_info: None,
                                    method_info: Some(LocalMethodName::new(
                                        receiver_type_name,
                                        Some(trait_name.to_string()),
                                        "fmt".to_string(),
                                    )),
                                },
                                type_args: vec![],
                                args: vec![fmt_mut_ref],
                            },
                            TypeTable::UNIT,
                            span,
                        );
                        stmts.push(TirStmt::new(TirStmtKind::Expr(fmt_call), span));
                    }
                }
            }
        }

        // break __tmpl: __r;
        let buf_final = TirExpr::new(
            TirExprKind::Local {
                index: buf_index,
                name: "__r".to_string(),
            },
            string_type,
            span,
        );
        stmts.push(TirStmt::new(
            TirStmtKind::Break {
                label: Some(label.clone()),
                value: Some(buf_final),
            },
            span,
        ));

        ctx.exit_scope();

        TirExpr::new(
            TirExprKind::LabeledBlock {
                label,
                block: TirBlock::new(stmts, span),
                result_type: string_type,
            },
            string_type,
            span,
        )
    }

    /// Parse a format specifier string like "05", "<10", "#x", ".2" etc.
    /// Syntax: `[[fill]align][sign][#][0][width][.precision]type`
    fn parse_format_spec(&self, spec: &str) -> ParsedFormatSpec {
        let chars: Vec<char> = spec.chars().collect();
        let len = chars.len();
        let mut i = 0;

        let mut fill = None;
        let mut align = None;
        let mut sign_plus = false;
        let mut alternate = false;
        let mut zero_pad = false;
        let mut width = None;
        let mut precision = None;
        let mut type_char = None;

        // Parse [fill][align]: fill is any char, align is '<', '^', '>'
        if i + 1 < len && matches!(chars[i + 1], '<' | '^' | '>') {
            fill = Some(chars[i]);
            align = Some(chars[i + 1]);
            i += 2;
        } else if i < len && matches!(chars[i], '<' | '^' | '>') {
            align = Some(chars[i]);
            i += 1;
        }

        // Parse [sign]: '+'
        if i < len && chars[i] == '+' {
            sign_plus = true;
            i += 1;
        }

        // Parse [#]: alternate form
        if i < len && chars[i] == '#' {
            alternate = true;
            i += 1;
        }

        // Parse [0]: zero-pad
        if i < len && chars[i] == '0' && (i + 1 >= len || chars[i + 1].is_ascii_digit()) {
            zero_pad = true;
            i += 1;
        }

        // Parse [width]: digits
        let width_start = i;
        while i < len && chars[i].is_ascii_digit() {
            i += 1;
        }
        if i > width_start {
            let w: String = chars[width_start..i].iter().collect();
            width = w.parse().ok();
        }

        // Parse [.precision]: '.' followed by digits
        if i < len && chars[i] == '.' {
            i += 1;
            let prec_start = i;
            while i < len && chars[i].is_ascii_digit() {
                i += 1;
            }
            if i > prec_start {
                let p: String = chars[prec_start..i].iter().collect();
                precision = p.parse().ok();
            } else {
                precision = Some(0);
            }
        }

        // Parse type: b, o, x, X, e, E, ?
        if i < len && matches!(chars[i], 'b' | 'o' | 'x' | 'X' | 'e' | 'E' | '?') {
            type_char = Some(chars[i]);
        }

        ParsedFormatSpec {
            fill,
            align,
            sign_plus,
            alternate,
            zero_pad,
            width,
            precision,
            type_char,
        }
    }

    /// Build a `Formatter::new(&mut __r)` or `Formatter { fill: ..., buf: &mut __r }` expression.
    fn build_formatter_expr(
        &mut self,
        buf_index: u32,
        string_type: TypeId,
        formatter_type: TypeId,
        parsed: &Option<ParsedFormatSpec>,
        span: Span,
    ) -> TirExpr {
        let mut_ref_string = self.type_table.borrow_mut().make_mut_ref(string_type);
        let buf_mut_ref = TirExpr::new(
            TirExprKind::Unary {
                op: TirUnaryOp::MutRef,
                expr: Box::new(TirExpr::new(
                    TirExprKind::Local {
                        index: buf_index,
                        name: "__r".to_string(),
                    },
                    string_type,
                    span,
                )),
            },
            mut_ref_string,
            span,
        );

        let has_custom_spec = parsed.as_ref().is_some_and(|p| {
            p.fill.is_some()
                || p.align.is_some()
                || p.sign_plus
                || p.alternate
                || p.zero_pad
                || p.width.is_some()
                || p.precision.is_some()
        });

        if !has_custom_spec {
            // Formatter::new(&mut __r)
            return TirExpr::new(
                TirExprKind::StaticCall {
                    func: FunctionRef::External {
                        module_source: ModuleSource::core("prelude/format.wado"),
                        name: "Formatter::new".to_string(),
                        monomorph_info: None,
                        method_info: Some(LocalMethodName::new(
                            "Formatter".to_string(),
                            None,
                            "new".to_string(),
                        )),
                    },
                    args: vec![buf_mut_ref],
                },
                formatter_type,
                span,
            );
        }

        // Construct Formatter struct literal with custom fields
        let pf = parsed.as_ref().unwrap();
        let alignment_type = self.type_table.borrow_mut().make_enum(
            "Alignment".to_string(),
            ModuleSource::core("prelude/format.wado"),
        );
        let fill_char = pf.fill.unwrap_or(if pf.zero_pad { '0' } else { ' ' });
        let align_index: u32 = match pf.align {
            Some('<') => 0, // Left
            Some('^') => 1, // Center
            _ => 2,         // Right (default)
        };
        let align_name = match align_index {
            0 => "Left",
            1 => "Center",
            _ => "Right",
        };

        TirExpr::new(
            TirExprKind::StructLiteral {
                struct_type: formatter_type,
                struct_name: "Formatter".to_string(),
                fields: vec![
                    TirStructField {
                        name: "fill".to_string(),
                        value: TirExpr::new(
                            TirExprKind::CharLiteral(fill_char),
                            TypeTable::CHAR,
                            span,
                        ),
                        field_index: 0,
                    },
                    TirStructField {
                        name: "align".to_string(),
                        value: TirExpr::new(
                            TirExprKind::EnumConstruct {
                                enum_type: alignment_type,
                                case_index: align_index,
                                case_name: align_name.to_string(),
                            },
                            alignment_type,
                            span,
                        ),
                        field_index: 1,
                    },
                    TirStructField {
                        name: "sign_plus".to_string(),
                        value: TirExpr::new(
                            TirExprKind::BoolLiteral(pf.sign_plus),
                            TypeTable::BOOL,
                            span,
                        ),
                        field_index: 2,
                    },
                    TirStructField {
                        name: "alternate".to_string(),
                        value: TirExpr::new(
                            TirExprKind::BoolLiteral(pf.alternate),
                            TypeTable::BOOL,
                            span,
                        ),
                        field_index: 3,
                    },
                    TirStructField {
                        name: "zero_pad".to_string(),
                        value: TirExpr::new(
                            TirExprKind::BoolLiteral(pf.zero_pad),
                            TypeTable::BOOL,
                            span,
                        ),
                        field_index: 4,
                    },
                    TirStructField {
                        name: "width".to_string(),
                        value: TirExpr::new(
                            TirExprKind::IntLiteral {
                                value: pf.width.unwrap_or(-1) as u64,
                                repr: pf.width.unwrap_or(-1).to_string(),
                            },
                            TypeTable::I32,
                            span,
                        ),
                        field_index: 5,
                    },
                    TirStructField {
                        name: "precision".to_string(),
                        value: TirExpr::new(
                            TirExprKind::IntLiteral {
                                value: pf.precision.unwrap_or(-1) as u64,
                                repr: pf.precision.unwrap_or(-1).to_string(),
                            },
                            TypeTable::I32,
                            span,
                        ),
                        field_index: 6,
                    },
                    TirStructField {
                        name: "buf".to_string(),
                        value: buf_mut_ref,
                        field_index: 7,
                    },
                ],
            },
            formatter_type,
            span,
        )
    }

    /// Determine the module source for a format trait impl (Display, Binary, etc.)
    fn resolve_display_impl_source(
        &self,
        base_type_id: TypeId,
        original_type_id: TypeId,
        trait_name: &str,
    ) -> (String, ModuleSource) {
        let (type_name, default_module) = match self.type_table.borrow().get(base_type_id).clone() {
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
                let prim_module = ModuleSource::Core {
                    name: "prelude/primitives.wado".to_string(),
                };
                (
                    self.type_table.borrow().mangle_type_name(base_type_id),
                    prim_module,
                )
            }
            _ => (
                self.type_table.borrow().mangle_type_name(original_type_id),
                self.current_module_source.clone(),
            ),
        };

        // Search for the actual module where `impl TraitName for TypeName` is defined,
        // since the trait impl may live in a different module than the type itself
        // (e.g., `impl Display for String` is in format.wado, not string.wado).
        for (module_src, module) in self.loaded_modules {
            for item in &module.items {
                if let Item::Impl(impl_block) = item
                    && let Some(trait_type) = &impl_block.trait_type
                {
                    let impl_type_name = self.get_type_name(&impl_block.ty);
                    let impl_trait_name = self.get_type_name(trait_type);
                    if impl_type_name == type_name && impl_trait_name == trait_name {
                        return (type_name, module_src.clone());
                    }
                }
            }
        }

        // Also check current module
        for item in &self.current_module_items {
            if let Item::Impl(impl_block) = item
                && let Some(trait_type) = &impl_block.trait_type
            {
                let impl_type_name = self.get_type_name(&impl_block.ty);
                let impl_trait_name = self.get_type_name(trait_type);
                if impl_type_name == type_name && impl_trait_name == trait_name {
                    return (type_name, self.current_module_source.clone());
                }
            }
        }

        (type_name, default_module)
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
                        let _ = self.logger.error(TypeError::TypeMismatch {
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

                        let method_info =
                            LocalMethodName::new(name.clone(), None, method_name.to_string());
                        let mangled_func_name = method_info.to_mangled_name();

                        return TirExpr::new(
                            TirExprKind::StaticCall {
                                func: FunctionRef::External {
                                    module_source: ModuleSource::core("prelude/int128.wado"),
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
                    let _ = self.logger.error(TypeError::InvalidLiteral {
                        message: format!("invalid u128 literal: {}", num_lit.repr),
                        span: lit.span,
                    });
                } else {
                    // i128
                    if let Ok(value) = Self::parse_i128_literal(&num_lit.repr) {
                        let (low, high) = Self::unpack_i128(value);
                        return self.build_from_pair_call(name, low, high, target_type, cast.span);
                    }
                    let _ = self.logger.error(TypeError::InvalidLiteral {
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
                let _ = self.logger.error(TypeError::InvalidLiteral {
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
                let method_info = LocalMethodName::new(name.clone(), None, method_name.to_string());
                let mangled_func_name = method_info.to_mangled_name();

                return TirExpr::new(
                    TirExprKind::StaticCall {
                        func: FunctionRef::External {
                            module_source: ModuleSource::core("prelude/int128.wado"),
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
        let source_type = expr.type_id;

        // Validate char casts: prohibit integer/float -> char (use char::from_u32 instead)
        // Exception: u8 -> char is always valid (0..255 are valid Unicode scalar values)
        let source_base = self.type_table.borrow().get_ultimate_base_type(source_type);
        let target_base = self.type_table.borrow().get_ultimate_base_type(target_type);
        if target_base == TypeTable::CHAR
            && source_base != TypeTable::CHAR
            && source_base != TypeTable::U8
        {
            let from_name = self.type_table.borrow().type_name(source_type);
            let _ = self.logger.error(TypeError::InvalidCast {
                from: from_name,
                to: "char".to_string(),
                hint: "use char::from_u32() or char::from_i32() for checked conversion".to_string(),
                span: cast.span,
            });
        }
        // char -> non-integer is invalid (char -> integer extracts code point)
        if source_base == TypeTable::CHAR
            && target_base != TypeTable::CHAR
            && !self.type_table.borrow().is_integer(target_base)
        {
            let to_name = self.type_table.borrow().type_name(target_type);
            let _ = self.logger.error(TypeError::InvalidCast {
                from: "char".to_string(),
                to: to_name,
                hint: "char can only be cast to integer types".to_string(),
                span: cast.span,
            });
        }

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
            let _ = self.logger.error(TypeError::TypeMismatch {
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
        // We need both the struct name (for struct_fields lookup) and module_source (for disambiguation)
        // Local struct definitions (current module) shadow imported/prelude structs.
        let (struct_name, symbol_module_source) = if self
            .struct_fields
            .get(name)
            .is_some_and(|info| info.module_source == self.current_module_source)
        {
            // Current module defines this struct locally - skip symbol table
            (name.clone(), None)
        } else if let Some(symbol) = self.symbols.lookup(name) {
            match &symbol.kind {
                crate::symbol::SymbolKind::Struct(_) => {
                    (symbol.name.clone(), Some(symbol.module_source.clone()))
                }
                _ => (name.clone(), None),
            }
        } else {
            // Fall back to local struct name
            (name.clone(), None)
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
                // Find expected field type for literal coercion
                // We use expected type for numeric literals (including negated ones)
                // and null literals to avoid interfering with tuple-to-array coercion
                // for generic struct fields
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

                let is_null_literal = matches!(
                    &field.value,
                    ast::Expr::Literal(lit) if matches!(&lit.value, ast::Literal::Null)
                );

                let expected_field_type = if is_numeric_literal || is_null_literal {
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

        // Get module_source for this struct
        // Priority: symbol table module_source > struct_fields > current_module_source
        // The symbol table module_source is needed for imported structs (especially with aliases)
        // to handle name collisions between local and imported structs
        let struct_module_source = if let Some(ms) = symbol_module_source {
            // Imported struct - use module_source from symbol table
            ms
        } else if let Some(info) = self.struct_fields.get(&struct_name) {
            // Local struct found in struct_fields
            info.module_source.clone()
        } else {
            // Fall back to current module
            self.current_module_source.clone()
        };

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
                    struct_module_source.clone(),
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
                    .make_struct(struct_name.clone(), struct_module_source.clone());
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
        let mut type_param_map: IndexMap<TypeId, TypeId> = IndexMap::new();

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
        type_param_map: &mut IndexMap<TypeId, TypeId>,
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
            let _ = self.logger.error(TypeError::UnknownType {
                name: format!("Self::{}", namespaced.name),
                span: namespaced.span,
            });
            return TypeTable::ERROR;
        }

        if namespaced.namespace.as_str() == "builtin" {
            if namespaced.name.as_str() == "array" {
                if namespaced.args.len() != 1 {
                    let _ = self.logger.error(TypeError::ArgumentCountMismatch {
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
                let _ = self.logger.error(TypeError::UnknownType {
                    name: format!("builtin::{}", namespaced.name),
                    span: namespaced.span,
                });
                TypeTable::ERROR
            }
        } else {
            let _ = self.logger.error(TypeError::UnknownType {
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

            // Check newtypes, struct definitions, and variants
            _ => {
                if let Some(&type_id) = self.newtypes.get(name) {
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
        let prelude_source = ModuleSource::core("prelude");

        match name {
            "Option" => {
                // Verify Option variant exists in symbol table (declared in prelude)
                // First check local imports, then fall back to prelude module
                let found_as_variant = self
                    .symbols
                    .lookup("Option")
                    .or_else(|| self.symbols.lookup_in_module(&prelude_source, "Option"))
                    .is_some_and(|s| matches!(s.kind, SymbolKind::Variant(_)));

                if !found_as_variant {
                    // Option not found as a variant - likely #![no_prelude] without explicit import
                    let _ = self.logger.error(TypeError::UnknownType {
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
                                        let _ =
                                            self.logger.error(TypeError::TraitBoundNotSatisfied {
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
pub fn resolve_module<H: CompilerHost>(
    module: &Module,
    module_source: ModuleSource,
    symbols: &SymbolTable,
    loaded_modules: &IndexMap<ModuleSource, Module>,
    logger: &Logger<H>,
) -> Result<TirModule, Bail> {
    let type_table = std::cell::RefCell::new(crate::tir::TypeTable::new());
    let builtin_registry = BuiltinRegistry::build_from_stdlib(&type_table);
    let mut resolver = Resolver::new(symbols, loaded_modules, &builtin_registry, logger);
    resolver.resolve_module(module, module_source)
}

/// Resolve all modules and return a Project ready for lowering.
///
/// This is the main entry point for the resolve phase. It resolves all modules
/// to TIR and packages them into a Project struct.
pub fn resolve_to_project<H: CompilerHost>(
    symbols: SymbolTable,
    modules: &IndexMap<ModuleSource, Module>,
    entry_module_source: ModuleSource,
    implicit_modules: IndexSet<ModuleSource>,
    module_name: String,
    logger: &Logger<H>,
) -> Result<Project, Bail> {
    let tir_modules =
        Resolver::resolve_all_modules(&symbols, modules, entry_module_source.clone(), logger)?;

    let (wasi_registry, world_registry) = crate::component_model::WasiRegistry::build_from_stdlib();

    // Build builtin registry (uses a temporary type table for type resolution)
    let temp_type_table = std::cell::RefCell::new(crate::tir::TypeTable::new());
    let builtin_registry =
        crate::builtin_registry::BuiltinRegistry::build_from_stdlib(&temp_type_table);

    Ok(Project::new(
        entry_module_source,
        tir_modules,
        symbols,
        implicit_modules,
        module_name,
        wasi_registry,
        world_registry,
        builtin_registry,
    ))
}
