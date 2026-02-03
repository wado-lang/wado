// Code generator for Wado
// Generates Component Model WebAssembly using wasm-encoder
// Targets WASI P3 (0.3.0-rc-2025-09-16) with native stream<T> types

use std::cell::RefCell;
use std::rc::Rc;

use crate::ast::Type;
use crate::builtin_registry::BuiltinFunctionInfo;
use crate::bundled::wado_bundled_wasm;
use crate::component_model::{
    CmPrimitiveType, WasiFunctionInfo, WasiInterfaceInfo, build_local_alias_name,
    return_type_requires_outptr, type_id_to_valtype, wasi_type_to_valtype,
};
use crate::copy_context::{ArrayCopyLocals, CopyContext};
use crate::name::{
    FreeFunctionName, FunctionId, MethodName, ModuleSource, StructName, build_core_internal_name,
    mangle_array_type, mangle_generic_name, mangle_result_type,
};
use crate::project::Project;
use crate::symbol::SymbolTable;
use crate::tir::{
    PrimitiveType, ResolvedType, TirBinaryOp, TirBlock, TirExpr, TirExprKind, TirFunction,
    TirImport, TirLiteralPattern, TirMatchArm, TirModule, TirPattern, TirStmt, TirStmtKind,
    TirUnaryOp, TypeId, TypeTable,
};
use crate::wasm_builder::{ComponentModelContext, CoreModuleBuilder, RecTypeKind};
use crate::wasm_postprocess;
use crate::world_registry::WorldExportInfo;
use heck::ToKebabCase;
use indexmap::IndexMap;
use std::collections::{HashMap, HashSet};
use wasm_encoder::{
    AbstractHeapType, Alias, BlockType, BranchHint, BranchHints, CanonicalOption, CodeSection,
    ComponentBuilder, ComponentExportKind, ComponentOuterAliasKind, ComponentValType, ConstExpr,
    DataCountSection, DataSection, ElementSection, Elements, ExportKind, ExportSection, FieldType,
    Function, FunctionSection, HeapType, InstanceType, Instruction, MemArg, MemorySection,
    MemoryType, Module, ModuleArg, NameMap, NameSection, PrimitiveValType, RefType, StorageType,
    TypeBounds, TypeSection, ValType,
};
use wasmparser::{Validator, WasmFeatures};

/// Helper to get the `ModuleSource` for String type (core:string)
fn string_module_source() -> ModuleSource {
    ModuleSource::core("string")
}

/// Information about a user-defined struct type
#[derive(Debug, Clone)]
struct StructTypeInfo {
    type_idx: u32,
    field_count: usize,
    /// Whether this struct is a monomorphized generic (e.g., Box<i32>)
    is_monomorphized: bool,
    /// Base generic struct name for monomorphized structs (e.g., "Box" for "Box<i32>")
    /// None for non-monomorphized structs
    base_name: Option<String>,
}

/// Parameters for building the main core module
struct BuildMainModuleParams<'a> {
    entry_tir: &'a TirModule,
    all_tir_modules: &'a IndexMap<ModuleSource, TirModule>,
    symbols: &'a SymbolTable,
    string_data: &'a [u8],
    project: &'a Project,
    module_name: &'a str,
    /// WASI functions that are available (lowered at component level)
    /// These are the local alias names (e.g., "`wasi:cli/Stdout::write_via_stream`")
    available_wasi_funcs: &'a HashSet<String>,
}

/// Code generator state.
///
/// Contains mutable state accumulated during code generation,
/// plus a reference to immutable project data (registries, symbols, etc.).
pub struct Codegen<'a> {
    /// Reference to the immutable project data
    project: &'a Project,
    string_literals: Vec<String>,
    /// Registry of user-defined struct types (keyed by `StructName` for type safety)
    struct_types: HashMap<StructName, StructTypeInfo>,
    /// Registry of tuple types (keyed by element `TypeIds`, maps to GC struct type index)
    tuple_types: HashMap<Vec<TypeId>, u32>,
    /// Registry of raw array types (keyed by element `TypeId`, maps to GC array type index)
    /// These are the underlying `builtin::array`<T> types used in Array<T>.repr
    array_types: HashMap<TypeId, u32>,
    /// Secondary lookup for array types by element type name (to handle duplicate `TypeIds`)
    array_types_by_name: HashMap<String, u32>,
    /// Registry of box types for primitive references (keyed by `ValType`, maps to GC struct type index)
    /// Box types are single-field mutable structs that allow references to primitives
    box_types: HashMap<ValType, u32>,
    /// Counter for generating unique closure IDs
    closure_counter: u32,
    /// Registry of closure struct types (env + funcref pair)
    /// Key: (`env_type_idx`, `fn_type_idx`)
    /// Value: `closure_struct_type_idx`
    #[allow(dead_code)]
    closure_struct_types: HashMap<(u32, u32), u32>,
    /// Registry of canonical closure types based on user-visible function signature.
    /// Used for function type parameters (e.g., `fn(i32) -> i32`).
    /// Key: (`param_type_ids`, `return_type_id`)
    /// Value: (`canonical_fn_type_idx`, `canonical_fn_type_name`, `canonical_closure_struct_type_idx`)
    canonical_closure_types: HashMap<(Vec<TypeId>, TypeId), (u32, String, u32)>,
    /// Canonical wrapper function indices for closure __call methods.
    /// Key: `functor_id` (from `ClosureToCanonical`)
    /// Value: wrapper function index (has canonical signature)
    closure_canonical_wrappers: HashMap<u32, u32>,
    /// Registry of custom variant types
    /// Key: variant name (e.g., "Shape")
    /// Value: `VariantTypeInfo` with struct type index and case metadata
    variant_types: HashMap<String, VariantTypeInfo>,
    /// Pre-allocated type indices for user types during rec group construction.
    /// This allows `type_id_to_valtype` to resolve forward references within a rec group.
    /// Cleared after the rec group is defined.
    pending_type_indices: HashMap<String, u32>,
}

/// Information about a single variant case's Wasm GC representation
#[derive(Clone, Debug)]
struct VariantCaseInfo {
    /// The case name (e.g., "Number", "Str")
    name: String,
    /// The GC struct type index for this specific case (subtype of base)
    type_idx: u32,
    /// Payload type for this case (None for unit variants)
    payload_type: Option<ValType>,
}

/// Information about a custom variant type's Wasm GC representation
///
/// Uses subtype-based representation where:
/// - Base type has only the discriminator (tag) field
/// - Each case is a subtype with case-specific payload fields
#[derive(Clone, Debug)]
struct VariantTypeInfo {
    /// The GC struct type index for the base variant type (discriminator only)
    base_type_idx: u32,
    /// Information about each case (indexed by `case_index`)
    cases: Vec<VariantCaseInfo>,
}

/// Info for generating canonical wrapper for closure __call methods
#[derive(Clone)]
struct ClosureCallWrapperInfo {
    /// Functor ID (from __`Closure_N`)
    functor_id: u32,
    /// Function index of the __call method
    call_func_idx: u32,
    /// Functor struct type index (for casting)
    functor_type_idx: u32,
    /// Parameter type IDs (excluding self)
    param_type_ids: Vec<TypeId>,
    /// Return type ID
    return_type_id: TypeId,
}

/// Context for tracking local variables during function code generation
/// Local indices in Wasm: parameters come first, then declared locals
struct FunctionContext {
    /// Map from variable name to local index
    locals: HashMap<String, u32>,
    /// Map from variable name to type (for type inference)
    local_type_map: HashMap<String, ValType>,
    /// Number of parameters (locals `0..param_count` are parameters)
    #[allow(dead_code)]
    param_count: u32,
    /// Next available local index for new variables
    next_local: u32,
    /// Local types for non-parameter locals (for function declaration)
    local_types: Vec<ValType>,
    /// Return type of the function (for `ref.as_non_null` handling)
    return_type: Option<ValType>,
    /// Pending branch hint from `builtin::likely()` or `builtin::unlikely()`
    /// None = no hint, Some(true) = likely taken, Some(false) = unlikely taken
    pending_branch_hint: Option<bool>,
    /// Collected branch hints for this function (offset, taken)
    branch_hints: Vec<(u32, bool)>,
    /// Module path of the current function (for access control checks)
    current_module_path: Vec<String>,
    /// Stack of break targets for loops and labeled blocks.
    /// Each entry contains: (label, `extra_depth`, `break_offset`, `is_loop`, `result_type`)
    /// - `label`: None for anonymous loop, Some(name) for labeled blocks/loops
    /// - `extra_depth`: incremented by if statements inside the block
    /// - `break_offset`: 1 for while/loop/labeled block, 2 for for loops
    /// - `is_loop`: true for loops, false for labeled blocks
    /// - `result_type`: `Some(type_id)` for labeled block expressions that return a value
    ///
    /// For break: use `break_offset` + `extra_depth`.
    /// For continue: use `extra_depth` (only valid for loops).
    loop_info: Vec<(Option<String>, u32, u32, bool, Option<TypeId>)>,
    /// Local indices that have their address taken (&x or &mut x).
    /// For mutable primitives, these locals store a box reference instead of the raw value.
    address_taken_locals: std::collections::HashSet<u32>,
    /// Map from local index to its box type index (for address-taken primitive locals)
    local_box_types: HashMap<u32, u32>,
    /// Offset to add to local indices (always 0, kept for potential future use)
    local_index_offset: u32,
    /// Counter for generating unique `IndirectCall` temp locals per closure struct type
    /// Key is the closure struct type index, value is the counter for that type
    indirect_call_counters: HashMap<u32, u32>,
    /// Counter for generating unique `LetPattern` temp locals
    let_pattern_counter: u32,
    /// Counter for generating unique match scrutinee locals
    match_scrutinee_counter: u32,
    /// Context for managing value copy scratch locals
    copy_context: CopyContext,
    /// When true, builtin calls returning tuples should NOT wrap in struct.new
    /// Used for tuple elision optimization when destructuring multi-value returns
    skip_tuple_wrap: bool,
    /// When true, this function is an async export and returns should use task-return
    is_async_export: bool,
    /// When true, the target world exports an HTTP handler (returns Result<Response, `ErrorCode`>)
    has_http_handler_export: bool,
}

impl FunctionContext {
    fn new(param_count: u32) -> Self {
        Self {
            locals: HashMap::new(),
            local_type_map: HashMap::new(),
            param_count,
            next_local: param_count,
            local_types: Vec::new(),
            return_type: None,
            pending_branch_hint: None,
            branch_hints: Vec::new(),
            current_module_path: Vec::new(),
            loop_info: Vec::new(),
            address_taken_locals: std::collections::HashSet::new(),
            local_box_types: HashMap::new(),
            local_index_offset: 0,
            indirect_call_counters: HashMap::new(),
            let_pattern_counter: 0,
            match_scrutinee_counter: 0,
            copy_context: CopyContext::new(),
            skip_tuple_wrap: false,
            is_async_export: false,
            has_http_handler_export: false,
        }
    }

    fn with_module_path(param_count: u32, module_path: Vec<String>) -> Self {
        Self {
            locals: HashMap::new(),
            local_type_map: HashMap::new(),
            param_count,
            next_local: param_count,
            local_types: Vec::new(),
            return_type: None,
            pending_branch_hint: None,
            branch_hints: Vec::new(),
            current_module_path: module_path,
            loop_info: Vec::new(),
            address_taken_locals: std::collections::HashSet::new(),
            local_box_types: HashMap::new(),
            local_index_offset: 0,
            indirect_call_counters: HashMap::new(),
            let_pattern_counter: 0,
            match_scrutinee_counter: 0,
            copy_context: CopyContext::new(),
            skip_tuple_wrap: false,
            is_async_export: false,
            has_http_handler_export: false,
        }
    }

    /// Reset let-pattern counter (called between pre-allocation and code generation phases)
    fn reset_let_pattern_counter(&mut self) {
        self.let_pattern_counter = 0;
    }

    /// Reset match-scrutinee counter (called between pre-allocation and code generation phases)
    fn reset_match_scrutinee_counter(&mut self) {
        self.match_scrutinee_counter = 0;
    }

    /// Get the next match scrutinee local name and increment counter
    fn next_match_scrutinee_local_name(&mut self, type_key: &str) -> String {
        let name = format!(
            "__match_scrutinee_{}_{}",
            type_key, self.match_scrutinee_counter
        );
        self.match_scrutinee_counter += 1;
        name
    }

    /// Get the next let-pattern temp local name and increment counter
    fn next_let_pattern_local_name(&mut self) -> String {
        let name = format!("__let_pattern_temp_{}", self.let_pattern_counter);
        self.let_pattern_counter += 1;
        name
    }

    fn set_return_type(&mut self, ty: ValType) {
        self.return_type = Some(ty);
    }

    /// Set a pending branch hint (from `builtin::likely/unlikely`)
    fn set_branch_hint(&mut self, taken: bool) {
        self.pending_branch_hint = Some(taken);
    }

    /// Consume pending branch hint and record it at the given offset
    fn consume_branch_hint(&mut self, offset: u32) {
        if let Some(taken) = self.pending_branch_hint.take() {
            self.branch_hints.push((offset, taken));
        }
    }

    /// Add a parameter (must be called before any locals)
    fn add_param(&mut self, name: &str, ty: ValType) {
        let index = self.locals.len() as u32;
        self.locals.insert(name.to_string(), index);
        self.local_type_map.insert(name.to_string(), ty);
    }

    /// Allocate a new local variable, or return existing if already allocated
    fn alloc_local(&mut self, name: &str, ty: ValType) -> u32 {
        // Return existing local if already allocated (for pre-allocated scratch locals)
        if let Some(&existing) = self.locals.get(name) {
            return existing;
        }
        // Make reference types nullable so they don't require initialization at function entry.
        // Wasm GC validation requires non-nullable ref locals to be definitely assigned before use,
        // but variables declared in control flow branches can't satisfy this requirement.
        let ty = match ty {
            ValType::Ref(ref_type) if !ref_type.nullable => ValType::Ref(RefType {
                nullable: true,
                heap_type: ref_type.heap_type,
            }),
            _ => ty,
        };
        let index = self.next_local;
        self.locals.insert(name.to_string(), index);
        self.local_type_map.insert(name.to_string(), ty);
        self.local_types.push(ty);
        self.next_local += 1;
        index
    }

    /// Get local index by name
    fn get_local(&self, name: &str) -> Option<u32> {
        self.locals.get(name).copied()
    }

    /// Get local types for function declaration (after params)
    fn get_local_decls(&self) -> Vec<(u32, ValType)> {
        // Group consecutive locals of the same type
        let mut decls: Vec<(u32, ValType)> = Vec::new();
        for ty in &self.local_types {
            if let Some((count, last_ty)) = decls.last_mut()
                && last_ty == ty
            {
                *count += 1;
                continue;
            }
            decls.push((1, *ty));
        }
        decls
    }
}

// CoreModuleBuilder and ComponentModelContext are in wasm_builder.rs

/// Convert a `snake_case` identifier to kebab-case for Component Model
fn to_kebab_case(name: &str) -> String {
    name.to_kebab_case()
}

/// Convert a Wado type to a Component Model primitive value type.
/// Used for type parameters in generic types like Array<T> and Option<T>.
fn wado_type_to_cm_primitive(ty: &Type) -> ComponentValType {
    match ty {
        Type::Named(named) => match named.name.as_str() {
            "i8" => ComponentValType::Primitive(PrimitiveValType::S8),
            "i16" => ComponentValType::Primitive(PrimitiveValType::S16),
            "i32" => ComponentValType::Primitive(PrimitiveValType::S32),
            "i64" => ComponentValType::Primitive(PrimitiveValType::S64),
            "u8" => ComponentValType::Primitive(PrimitiveValType::U8),
            "u16" => ComponentValType::Primitive(PrimitiveValType::U16),
            "u32" => ComponentValType::Primitive(PrimitiveValType::U32),
            "u64" => ComponentValType::Primitive(PrimitiveValType::U64),
            "f32" => ComponentValType::Primitive(PrimitiveValType::F32),
            "f64" => ComponentValType::Primitive(PrimitiveValType::F64),
            "bool" => ComponentValType::Primitive(PrimitiveValType::Bool),
            "char" => ComponentValType::Primitive(PrimitiveValType::Char),
            "String" => ComponentValType::Primitive(PrimitiveValType::String),
            _ => panic!("unsupported Wado primitive type for CM: {}", named.name),
        },
        _ => panic!("unsupported Wado type for CM primitive: {ty:?}"),
    }
}

/// A type declaration that can be either a struct or a variant.
/// Used for unified topological sorting of type declarations.
enum TypeDecl<'a> {
    Struct(&'a crate::tir::TirStruct),
    Variant(&'a crate::tir::TirVariantDecl),
}

// =========================================================================
// br_table optimization constants and types
// =========================================================================
//
// Thresholds based on GCC/LLVM:
// - GCC: 5 cases minimum (--param case-values-threshold)
// - LLVM: 40% density threshold
//
// We use more conservative values for Wasm:
// - MIN_CASES: 8
// - DENSITY_THRESHOLD: 75%
// - MAX_RANGE: 1024

/// Minimum number of cases to consider `br_table` optimization
const BR_TABLE_MIN_CASES: usize = 8;

/// Minimum density (cases / range) to use `br_table` (75% = 0.75)
const BR_TABLE_DENSITY_THRESHOLD: f64 = 0.75;

/// Maximum range size for `br_table` (avoid huge tables)
const BR_TABLE_MAX_RANGE: i64 = 1024;

/// Analysis result for `br_table` optimization
struct BrTableAnalysis {
    /// Minimum value in the match
    min_value: i64,
    /// Maximum value in the match
    max_value: i64,
    /// Mapping from value to arm index
    value_to_arm: Vec<(i64, usize)>,
    /// Index of the default/wildcard arm (if any)
    default_arm: Option<usize>,
    /// Whether the scrutinee is i64 (vs i32)
    is_i64: bool,
}

/// Result of resolving a newtype chain to its ultimate base
enum UltimateBaseType {
    Struct {
        name: String,
        module_source: ModuleSource,
    },
    Primitive(crate::tir::PrimitiveType),
}

impl Codegen<'_> {
    /// Generate Component Model binary Wasm from a Project.
    ///
    /// This is the main entry point for code generation. Takes a reference to
    /// the Project for immutable data access.
    pub fn generate_wasm(project: &Project) -> Vec<u8> {
        // Collect string literals from all TIR modules
        let mut string_literals = Vec::new();
        for tir_module in project.tir_modules.values() {
            for s in &tir_module.string_literals {
                if !string_literals.contains(s) {
                    string_literals.push(s.clone());
                }
            }
        }

        // Create codegen with reference to project
        let mut codegen = Codegen {
            project,
            string_literals,
            struct_types: HashMap::new(),
            tuple_types: HashMap::new(),
            array_types: HashMap::new(),
            array_types_by_name: HashMap::new(),
            box_types: HashMap::new(),
            closure_counter: 0,
            closure_struct_types: HashMap::new(),
            canonical_closure_types: HashMap::new(),
            closure_canonical_wrappers: HashMap::new(),
            variant_types: HashMap::new(),
            pending_type_indices: HashMap::new(),
        };

        // Generate binary Wasm from TIR
        let wasm = codegen.generate_component();

        // Validate the generated Wasm
        Self::validate_wasm(&wasm);

        wasm
    }

    /// Validate generated Wasm binary using wasmparser
    ///
    /// This catches codegen bugs early by ensuring the output is valid Wasm.
    /// Panics if validation fails, as this indicates a compiler bug.
    fn validate_wasm(wasm: &[u8]) {
        let mut validator = Validator::new_with_features(WasmFeatures::all());
        if let Err(e) = validator.validate_all(wasm) {
            panic!(
                "Internal compiler error: generated invalid Wasm\n\
                 This is a bug in the Wado compiler. Please report it.\n\
                 Validation error: {e}"
            );
        }
    }

    /// Look up a struct type by name and module source.
    /// Tries qualified `StructName` first, falls back to entry point module.
    fn lookup_struct_type(
        &self,
        name: &str,
        module_source: &ModuleSource,
    ) -> Option<&StructTypeInfo> {
        // Try qualified name first
        let qualified = StructName::new(module_source.clone(), name.to_string());
        if let Some(info) = self.struct_types.get(&qualified) {
            return Some(info);
        }
        // Fall back to entry point module
        let simple = StructName::from_name(name);
        self.struct_types.get(&simple)
    }

    /// Extract struct names that a type depends on (for field types)
    /// Returns mangled names for `GenericInstance` types (e.g., "`BTreeNode`<String,i32>")
    /// Get type dependencies (struct and variant names) for a given type.
    /// Used for topological sorting of type declarations.
    fn get_type_dependencies(type_table: &TypeTable, type_id: TypeId) -> Vec<String> {
        match type_table.get(type_id) {
            ResolvedType::Struct { name, .. } => vec![name.clone()],
            ResolvedType::Variant { name, .. } => vec![name.clone()],
            ResolvedType::GenericInstance { type_args, .. } => {
                // Get dependencies from type arguments
                // Use mangled name for the generic instance (e.g., "BTreeNode<String,i32>")
                let mangled_name = type_table.mangle_type_name(type_id);
                let mut deps = vec![mangled_name];
                for arg in type_args {
                    deps.extend(Self::get_type_dependencies(type_table, *arg));
                }
                deps
            }
            ResolvedType::BuiltinArray(inner)
            | ResolvedType::Option(inner)
            | ResolvedType::Ref(inner)
            | ResolvedType::MutRef(inner)
            | ResolvedType::Stream(inner)
            | ResolvedType::Future(inner)
            | ResolvedType::Reactive(inner) => Self::get_type_dependencies(type_table, *inner),
            ResolvedType::Result { ok, err } => {
                let mut deps = Self::get_type_dependencies(type_table, *ok);
                deps.extend(Self::get_type_dependencies(type_table, *err));
                deps
            }
            ResolvedType::Tuple(elems) => elems
                .iter()
                .flat_map(|e| Self::get_type_dependencies(type_table, *e))
                .collect(),
            _ => vec![],
        }
    }

    /// Check if a struct has self-referential fields (directly or through Array/Ref/MutRef).
    /// Returns the list of field type IDs that create the self-reference cycle.
    fn get_self_referential_field_types(
        struct_name: &str,
        tir_struct: &crate::tir::TirStruct,
        type_table: &TypeTable,
    ) -> Vec<TypeId> {
        let mut self_ref_fields = Vec::new();
        for field in &tir_struct.fields {
            if Self::type_references_struct(field.type_id, struct_name, type_table) {
                self_ref_fields.push(field.type_id);
            }
        }
        self_ref_fields
    }

    /// Check if a type references a struct by name (transitively through Array/Ref/MutRef).
    /// The `struct_name` should be the full mangled name (e.g., "`AANode`<String,i32>").
    fn type_references_struct(type_id: TypeId, struct_name: &str, type_table: &TypeTable) -> bool {
        match type_table.get(type_id) {
            ResolvedType::Struct { name, .. } => {
                // Exact name match only
                name == struct_name
            }
            ResolvedType::GenericInstance { type_args, .. } => {
                // Check if this GenericInstance IS the struct we're looking for.
                // E.g., Node<String> is represented as GenericInstance { name: "Node", type_args: [String] }
                // and we need to check if "Node<String>" matches struct_name.
                let mangled_name = type_table.mangle_type_name(type_id);
                if mangled_name == struct_name {
                    return true;
                }
                // Also recurse into type args for Array<&mut Node<String>> patterns
                type_args
                    .iter()
                    .any(|arg| Self::type_references_struct(*arg, struct_name, type_table))
            }
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
                Self::type_references_struct(*inner, struct_name, type_table)
            }
            ResolvedType::BuiltinArray(inner) => {
                Self::type_references_struct(*inner, struct_name, type_table)
            }
            ResolvedType::Option(inner) => {
                Self::type_references_struct(*inner, struct_name, type_table)
            }
            _ => false,
        }
    }

    /// Sort structs and variants together topologically so dependencies are registered before dependents.
    /// This handles mutual dependencies between structs and variants (e.g., struct with variant field,
    /// variant with struct payload).
    fn sort_types_topologically<'b>(
        structs: &'b [crate::tir::TirStruct],
        variants: &'b [crate::tir::TirVariantDecl],
        type_table: &TypeTable,
    ) -> Vec<TypeDecl<'b>> {
        // Collect all type names
        let struct_names: HashSet<String> = structs.iter().map(|s| s.name.clone()).collect();
        let variant_names: HashSet<String> = variants.iter().map(|v| v.name.clone()).collect();
        let all_names: HashSet<String> = struct_names.union(&variant_names).cloned().collect();

        // Build dependency graph: deps[A] = [B] means A depends on B (B must come before A)
        let mut deps: HashMap<String, Vec<String>> = HashMap::new();

        // Add struct dependencies
        for s in structs {
            let mut type_deps = Vec::new();
            for field in &s.fields {
                let field_deps = Self::get_type_dependencies(type_table, field.type_id);
                for dep in field_deps {
                    // Only count dependencies on types in our set
                    if all_names.contains(&dep) && dep != s.name {
                        type_deps.push(dep);
                    }
                }
            }
            deps.insert(s.name.clone(), type_deps);
        }

        // Add variant dependencies (from payload types)
        for v in variants {
            let mut type_deps = Vec::new();
            for case in &v.cases {
                // Each variant case has exactly one payload type
                let payload_deps = Self::get_type_dependencies(type_table, case.payload);
                for dep in payload_deps {
                    if all_names.contains(&dep) && dep != v.name {
                        type_deps.push(dep);
                    }
                }
            }
            deps.insert(v.name.clone(), type_deps);
        }

        // Topological sort using Kahn's algorithm
        let mut in_degree: HashMap<String, usize> = HashMap::new();
        for name in &all_names {
            let type_deps = deps.get(name).map(std::vec::Vec::len).unwrap_or(0);
            in_degree.insert(name.clone(), type_deps);
        }

        // Build reverse mapping: dependents[B] = list of types that depend on B
        let mut dependents: HashMap<String, Vec<String>> = HashMap::new();
        for (name, type_deps) in &deps {
            for dep in type_deps {
                dependents
                    .entry(dep.clone())
                    .or_default()
                    .push(name.clone());
            }
        }

        // Start with types that have no dependencies
        let mut queue: Vec<String> = in_degree
            .iter()
            .filter(|&(_, deg)| *deg == 0)
            .map(|(name, _)| name.clone())
            .collect();

        let mut sorted_names = Vec::new();
        while let Some(name) = queue.pop() {
            sorted_names.push(name.clone());
            if let Some(deps_on_name) = dependents.get(&name) {
                for dependent in deps_on_name {
                    let deg = in_degree.get_mut(dependent).unwrap();
                    *deg -= 1;
                    if *deg == 0 {
                        queue.push(dependent.clone());
                    }
                }
            }
        }

        // Map names back to TypeDecl
        let name_to_struct: HashMap<&str, &crate::tir::TirStruct> =
            structs.iter().map(|s| (s.name.as_str(), s)).collect();
        let name_to_variant: HashMap<&str, &crate::tir::TirVariantDecl> =
            variants.iter().map(|v| (v.name.as_str(), v)).collect();

        sorted_names
            .iter()
            .filter_map(|name| {
                if let Some(s) = name_to_struct.get(name.as_str()) {
                    Some(TypeDecl::Struct(s))
                } else {
                    name_to_variant
                        .get(name.as_str())
                        .map(|v| TypeDecl::Variant(v))
                }
            })
            .collect()
    }

    /// Build main core module from TIR
    /// Build the main core Wasm module containing user-defined functions.
    fn build_main_module(&mut self, params: BuildMainModuleParams<'_>) -> Vec<u8> {
        let BuildMainModuleParams {
            entry_tir,
            all_tir_modules,
            symbols,
            string_data,
            project,
            module_name,
            available_wasi_funcs,
        } = params;
        let strip_names = project.strip_names;

        let mut module = Module::new();
        let mut builder = CoreModuleBuilder::new();
        let type_table = &*entry_tir.type_table.borrow();
        let entry_module_source = &entry_tir.module_source;

        // Collect ALL functions from loaded TIR modules (core:*, etc.)
        // We need to include all functions because they may have transitive dependencies
        // Format: (module_source, tir_func, type_table, qualified_name)
        // Note: We store Rc<RefCell<...>> to avoid lifetime issues with temporary borrows
        let mut loaded_funcs: Vec<(
            ModuleSource,
            Rc<RefCell<TirFunction>>,
            Rc<RefCell<TypeTable>>,
            String,
        )> = Vec::new();
        for (module_source, tir_mod) in all_tir_modules {
            // Skip entry module (handled separately)
            if module_source == entry_module_source {
                continue;
            }
            // Skip wasi:* modules (they only contain effect declarations)
            if module_source.is_wasi() {
                continue;
            }
            for func_rc in &tir_mod.functions {
                let tir_func = func_rc.borrow();
                // Skip run function
                if tir_func.name == "run" {
                    continue;
                }
                // Note: We no longer skip non-pub functions unconditionally.
                // Non-pub functions may be called by pub functions (e.g., __initialize_module
                // calling module-private initialization helpers). The reachability check
                // below will determine if the function should be included.
                // Skip bodyless functions
                if tir_func.body.is_none() {
                    continue;
                }
                // Skip methods (names containing "::") - they're handled in loaded_methods
                if tir_func.name.contains("::") {
                    continue;
                }
                // Skip functions with unsupported effects
                // Supported effects: Stdout, Stderr, MonotonicClock, Environment
                // Exit is only supported if explicitly used (runtime may not support it)
                if !tir_func.effects.is_empty() {
                    let exit_available = project.has_effect("Exit");
                    let has_unsupported_effects = tir_func.effects.iter().any(|e| {
                        let effect_name = e.as_str();
                        // Exit effect requires explicit usage tracking
                        if effect_name == "Exit" {
                            return !exit_available;
                        }
                        !matches!(
                            effect_name,
                            "Stdout" | "Stderr" | "MonotonicClock" | "Environment"
                        )
                    });
                    if has_unsupported_effects {
                        continue;
                    }
                }
                let func_id = FunctionId::Free(FreeFunctionName::from_module_source(
                    module_source,
                    &tir_func.name,
                ));
                // Skip functions not reachable from entry point (DCE)
                if !project.is_reachable(&func_id) {
                    continue;
                }
                let mangled_name = func_id.to_string();
                drop(tir_func); // Release borrow before cloning Rc
                loaded_funcs.push((
                    module_source.clone(),
                    Rc::clone(func_rc),
                    Rc::clone(&tir_mod.type_table),
                    mangled_name,
                ));
            }
        }

        // Collect main module struct names first (for collision detection)
        let main_module_struct_names: std::collections::HashSet<String> =
            entry_tir.structs.iter().map(|s| s.name.clone()).collect();

        // Collect impl methods from loaded TIR modules
        // Note: With the current TIR design, methods are added as regular functions
        // (with mangled names like "Point::sum") in tir_mod.functions, not in impls.
        // This loop is kept for future when impls may be populated.
        // Format: (module_source, struct_lookup_name, tir_func, type_table, mangled_name)
        let mut loaded_methods: Vec<(
            ModuleSource,
            StructName,
            Rc<RefCell<TirFunction>>,
            Rc<RefCell<TypeTable>>,
            String,
        )> = Vec::new();
        for (module_source, tir_mod) in all_tir_modules {
            // Skip entry module (handled separately)
            if module_source == entry_module_source {
                continue;
            }
            // Skip wasi:* modules
            if module_source.is_wasi() {
                continue;
            }
            // Methods are stored as functions with method_info metadata
            // (resolver adds them to functions, not impls)
            for func_rc in &tir_mod.functions {
                let func = func_rc.borrow();
                // Check if this is a method using metadata
                if let Some(ref info) = func.method_info {
                    let struct_name = &info.struct_name;
                    let trait_name = info.trait_name.as_deref();
                    let method_name = &info.method_name;

                    // Skip non-pub methods (except monomorphized ones which are generated for
                    // concrete instantiation sites and must be included)
                    if !func.is_pub && func.monomorph_info.is_none() {
                        continue;
                    }
                    // Skip bodyless methods
                    if func.body.is_none() {
                        continue;
                    }
                    // Skip methods that contain type parameters (from generic structs like Box<T>)
                    // These methods need to be inlined at call sites with concrete types
                    let type_table = &*tir_mod.type_table.borrow();
                    let has_type_params = type_table.contains_type_param(func.return_type)
                        || func
                            .params
                            .iter()
                            .any(|p| type_table.contains_type_param(p.type_id));
                    if has_type_params {
                        continue;
                    }
                    // Build function ID for DCE check: path/Struct^Trait::method or path/Struct::method
                    let method_id = FunctionId::Method(MethodName::from_module_source(
                        module_source,
                        struct_name,
                        trait_name,
                        method_name,
                    ));
                    // Skip methods not reachable from entry point (DCE)
                    // Exception: monomorphized methods are generated by the lower pass
                    // after optimization, so the optimizer doesn't know about them.
                    // They are reachable if they were generated.
                    let is_monomorphized = func.monomorph_info.is_some();
                    if !is_monomorphized && !project.is_reachable(&method_id) {
                        continue;
                    }
                    let method_mangled = method_id.to_string();
                    // Determine struct lookup name - use qualified name if there's a collision
                    let struct_lookup_name = if main_module_struct_names.contains(struct_name) {
                        // Collision - use qualified StructName
                        StructName::new(module_source.clone(), struct_name.clone())
                    } else {
                        // No collision - use simple StructName (entry point)
                        StructName::new(ModuleSource::entry_point(), struct_name.clone())
                    };
                    // Use the same fully mangled name for registration
                    // This ensures consistency between DCE tracking and codegen
                    drop(func); // Release borrow before cloning Rc
                    loaded_methods.push((
                        module_source.clone(),
                        struct_lookup_name,
                        Rc::clone(func_rc),
                        Rc::clone(&tir_mod.type_table),
                        method_mangled,
                    ));
                }
            }
        }

        // Build import name → qualified name lookup table for call resolution
        let mut _import_lookup: HashMap<String, String> = HashMap::new();
        for (module_source, tir_func_rc, _, qualified_name) in &loaded_funcs {
            if *module_source != ModuleSource::entry_point() {
                _import_lookup.insert(tir_func_rc.borrow().name.clone(), qualified_name.clone());
            }
        }

        // ========================================
        // Define types using the builder
        // ========================================

        // Builtin function types - derived from core/builtin.wado
        // DCE: Only define types for builtins that are actually used (via TIR imports)
        // Build set of imported canonical names for quick lookup
        let imported_canonical_names: HashSet<&str> = entry_tir
            .imports
            .iter()
            .map(|i| i.canonical_name.as_str())
            .collect();
        for func in self.project.builtin_registry.imported_builtins() {
            let canonical_name = func.canonical_name.as_ref().unwrap();
            // Skip if this builtin is not used
            if !imported_canonical_names.contains(canonical_name.as_str()) {
                continue;
            }
            // For Service world, task-return needs different signature
            // result<own<response>, error-code> flattens based on the error-code variant payloads.
            // The full error-code with record payloads flattens to:
            // (i32, i32, i32, i64, i32, i32, i32, i32)
            // - i32: Ok/Err discriminant
            // - i32: Response handle (Ok) or error-code discriminant (Err)
            // - Remaining: Space for largest error-code payload (records with option<string>, etc.)
            if canonical_name == "task-return" && self.project.has_http_handler_export {
                builder.define_func_type(
                    canonical_name,
                    &[
                        ValType::I32, // Ok/Err discriminant
                        ValType::I32, // Response handle or error discriminant
                        ValType::I32, // Payload field
                        ValType::I64, // u64 payload (option<u64>)
                        ValType::I32, // Payload field
                        ValType::I32, // Payload field
                        ValType::I32, // Payload field
                        ValType::I32, // Payload field
                    ],
                    &[],
                );
                continue;
            }
            let params = self.builtin_func_to_core_params(func);
            let results = self.builtin_func_to_core_results(func);
            builder.define_func_type(canonical_name, &params, &results);
        }

        // Define HTTP function types for Service world
        if self.project.has_http_handler_export {
            // [constructor]fields: () -> i32 (resource handle)
            builder.define_func_type("http-fields-constructor", &[], &[ValType::I32]);

            // [static]response.new:
            // (headers: i32, contents_discrim: i32, contents_stream: i32,
            //  trailers_future: i32, out_ptr: i32) -> ()
            // The function writes the result (response handle, transmission future) to out_ptr
            // Actually, for lowered functions with multi-value results, the returns go to linear memory
            // Need to check the actual signature from wasmtime
            builder.define_func_type(
                "http-response-new",
                &[
                    ValType::I32, // headers (fields handle)
                    ValType::I32, // contents discriminant (0=None, 1=Some)
                    ValType::I32, // contents stream handle (if Some)
                    ValType::I32, // trailers future handle
                    ValType::I32, // out pointer for multi-value result
                ],
                &[],
            );
        }

        // Register PRIMITIVE array types first (elements are primitives)
        // These don't depend on struct types
        self.register_primitive_array_types_from_table(type_table, &mut builder);
        for (module_source, tir_mod) in all_tir_modules {
            if module_source != entry_module_source {
                self.register_primitive_array_types_from_table(
                    &tir_mod.type_table.borrow(),
                    &mut builder,
                );
            }
        }

        // Register box types BEFORE struct types because:
        // Option<primitive> fields need to map to nullable box references
        self.register_box_types(&mut builder, project);

        // PHASE 1: Register NON-MONOMORPHIZED structs AND variants from library modules
        // These are "base" structs like String that don't depend on array types
        // - If no collision: register with simple name (entry point)
        // - If collision with main module: register with qualified name (full module source)
        // Note: all_tir_modules is in topological order (dependency modules first)
        // Note: Structs and variants must be registered together with topological sorting
        //       because structs may have variant fields and variants may have struct payloads.
        for (module_source, tir_mod) in all_tir_modules {
            // Skip entry module (handled separately in PHASE 2)
            if module_source == entry_module_source {
                continue;
            }
            // Collect non-generic, non-monomorphized public structs
            let lib_structs: Vec<_> = tir_mod
                .structs
                .iter()
                .filter(|s| {
                    s.is_pub
                        && s.type_params.is_empty()
                        && s.monomorph_info.is_none()
                        && !self.struct_contains_type_params(s, &tir_mod.type_table.borrow())
                })
                .cloned()
                .collect();
            // Collect non-generic public variants
            let lib_variants: Vec<_> = tir_mod
                .variants
                .iter()
                .filter(|v| v.is_pub && v.type_params.is_empty())
                .cloned()
                .collect();
            // Sort structs and variants together topologically
            let sorted_types = Self::sort_types_topologically(
                &lib_structs,
                &lib_variants,
                &tir_mod.type_table.borrow(),
            );
            for type_decl in sorted_types {
                match type_decl {
                    TypeDecl::Struct(tir_struct) => {
                        let struct_name = if main_module_struct_names.contains(&tir_struct.name) {
                            // Collision - use qualified name with full module source
                            StructName::new(module_source.clone(), tir_struct.name.clone())
                        } else {
                            // No collision - use simple name (entry point)
                            StructName::new(ModuleSource::entry_point(), tir_struct.name.clone())
                        };
                        self.register_struct_type(
                            struct_name,
                            tir_struct,
                            &tir_mod.type_table.borrow(),
                            &mut builder,
                        );
                    }
                    TypeDecl::Variant(tir_variant) => {
                        self.register_variant_type(
                            tir_variant,
                            &tir_mod.type_table.borrow(),
                            &mut builder,
                        );
                    }
                }
            }
        }

        // Register tuple types BEFORE PHASE 2 so variant payloads have concrete tuple types
        // instead of fallback (ref struct). Tuples are simple structs that don't depend on
        // any other types.
        self.register_tuple_types_from_table(type_table, &mut builder);
        for (module_source, tir_mod) in all_tir_modules {
            if module_source != entry_module_source {
                self.register_tuple_types_from_table(&tir_mod.type_table.borrow(), &mut builder);
            }
        }

        // PHASE 2: Register NON-MONOMORPHIZED main module structs AND variants
        // Skip generic templates and monomorphized types
        // Sort structs and variants together topologically to handle mutual dependencies
        // (e.g., struct with variant field, variant with struct payload)
        // Note: Non-mono structs that depend on mono structs will be deferred to PHASE 4
        let non_mono_structs: Vec<_> = entry_tir
            .structs
            .iter()
            .filter(|s| {
                s.type_params.is_empty()
                    && s.monomorph_info.is_none()
                    && !self.struct_contains_type_params(s, type_table)
            })
            .cloned()
            .collect();
        let non_mono_variants: Vec<_> = entry_tir
            .variants
            .iter()
            .filter(|v| v.type_params.is_empty())
            .cloned()
            .collect();
        let sorted_types =
            Self::sort_types_topologically(&non_mono_structs, &non_mono_variants, type_table);
        // Track which non-mono structs depend on mono structs (to be deferred to PHASE 4)
        let mono_struct_names: HashSet<String> = entry_tir
            .structs
            .iter()
            .filter(|s| s.type_params.is_empty() && s.monomorph_info.is_some())
            .map(|s| s.name.clone())
            .collect();
        let mut deferred_non_mono_structs: Vec<crate::tir::TirStruct> = Vec::new();
        for type_decl in sorted_types {
            match type_decl {
                TypeDecl::Struct(tir_struct) => {
                    // Check if this struct depends on any mono structs
                    let deps = tir_struct
                        .fields
                        .iter()
                        .flat_map(|f| Self::get_type_dependencies(type_table, f.type_id))
                        .collect::<HashSet<_>>();
                    if deps.iter().any(|d| mono_struct_names.contains(d)) {
                        // Defer to PHASE 4
                        deferred_non_mono_structs.push(tir_struct.clone());
                    } else {
                        let struct_name =
                            StructName::new(ModuleSource::entry_point(), tir_struct.name.clone());
                        self.register_struct_type(
                            struct_name,
                            tir_struct,
                            type_table,
                            &mut builder,
                        );
                    }
                }
                TypeDecl::Variant(tir_variant) => {
                    self.register_variant_type(tir_variant, type_table, &mut builder);
                }
            }
        }

        // Register struct type aliases (e.g., `Point as OtherPoint`)
        for (alias_name, alias_module_path, original_name) in symbols.get_struct_aliases() {
            let alias_struct_name =
                StructName::new(ModuleSource::entry_point(), alias_name.clone());
            // Check if there's a collision (main module has same-named struct)
            if main_module_struct_names.contains(&original_name) && !alias_module_path.is_empty() {
                // Collision case - use qualified name from the alias's module
                let qualified_name =
                    StructName::from_path_and_name(&alias_module_path, &original_name);
                if let Some(info) = self.struct_types.get(&qualified_name).cloned() {
                    self.struct_types.insert(alias_struct_name, info);
                }
            } else {
                // No collision - use simple name (empty module path)
                let original_struct_name =
                    StructName::new(ModuleSource::entry_point(), original_name.clone());
                if let Some(info) = self.struct_types.get(&original_struct_name).cloned() {
                    self.struct_types.insert(alias_struct_name, info);
                }
            }
        }

        // PHASE 2.5: Register arrays of non-monomorphized structs
        // This must happen BEFORE monomorphized struct registration because
        // monomorphized structs (e.g., BTreeNode<String, i32>) may have fields
        // that are arrays of non-monomorphized structs (e.g., Array<String>).
        self.register_non_monomorphized_struct_arrays(type_table, &mut builder);
        for (module_source, tir_mod) in all_tir_modules {
            if module_source != entry_module_source {
                self.register_non_monomorphized_struct_arrays(
                    &tir_mod.type_table.borrow(),
                    &mut builder,
                );
            }
        }

        // Register canonical closure types BEFORE monomorphized struct types.
        // This must happen after non-monomorphized structs (which closures may return)
        // but before monomorphized structs (which may have function-typed fields like
        // MapIter<T,U> with f: fn(T) -> U).
        self.register_canonical_closure_types_from_table(type_table, &mut builder);
        for (module_source, tir_mod) in all_tir_modules {
            if module_source != entry_module_source {
                self.register_canonical_closure_types_from_table(
                    &tir_mod.type_table.borrow(),
                    &mut builder,
                );
            }
        }

        // PHASE 3: Register MONOMORPHIZED structs from library modules
        // These must be registered BEFORE array types because array types with
        // generic struct elements (e.g., Array<Pair<i32, String>>) need to call
        // type_id_to_valtype which requires the struct to be registered.
        // Note: We register ALL monomorphized structs (not just public ones) because
        // private structs like TreeMapNode may be needed as dependencies of public
        // structs like TreeMap.
        // Note: Array<T> is now treated like any other generic struct and goes through
        // this normal registration flow.
        for (module_source, tir_mod) in all_tir_modules {
            if module_source == entry_module_source {
                continue;
            }
            // Collect monomorphized structs
            let mono_lib_structs: Vec<_> = tir_mod
                .structs
                .iter()
                .filter(|s| s.monomorph_info.is_some())
                .cloned()
                .collect();

            // Sort topologically to ensure dependencies come before dependents
            let lib_type_table = tir_mod.type_table.borrow();
            let sorted_lib_types =
                Self::sort_types_topologically(&mono_lib_structs, &[], &lib_type_table);

            for type_decl in sorted_lib_types {
                let TypeDecl::Struct(tir_struct) = type_decl else {
                    continue;
                };
                let struct_name =
                    StructName::new(ModuleSource::entry_point(), tir_struct.name.clone());
                // Check for self-referential structs using full struct name
                let self_ref_fields = Self::get_self_referential_field_types(
                    &tir_struct.name,
                    tir_struct,
                    &lib_type_table,
                );
                if self_ref_fields.is_empty() {
                    self.register_struct_type(
                        struct_name,
                        tir_struct,
                        &lib_type_table,
                        &mut builder,
                    );
                } else {
                    self.register_self_referential_struct(
                        struct_name,
                        tir_struct,
                        &self_ref_fields,
                        &lib_type_table,
                        &mut builder,
                    );
                }
            }
        }

        // PHASE 3.5: Pre-register array types from monomorphized struct fields
        // This must happen BEFORE PHASE 4 so that struct fields with Array<Tuple<...>>
        // or other non-primitive element types can be properly typed.
        self.pre_register_arrays_from_monomorphized_structs(entry_tir, type_table, &mut builder);
        for (module_source, tir_mod) in all_tir_modules {
            if module_source == entry_module_source {
                continue;
            }
            self.pre_register_arrays_from_monomorphized_structs(
                tir_mod,
                &tir_mod.type_table.borrow(),
                &mut builder,
            );
        }

        // PHASE 4: Register MONOMORPHIZED main module structs AND deferred non-mono structs
        // Deferred non-mono structs are those that depend on mono structs (e.g., Container with Box<i32>)
        // Note: Array<T> is now treated like any other generic struct.
        let mono_structs: Vec<_> = entry_tir
            .structs
            .iter()
            .filter(|s| s.type_params.is_empty() && s.monomorph_info.is_some())
            .cloned()
            .collect();
        // Combine mono structs with deferred non-mono structs
        let all_phase4_structs: Vec<_> = mono_structs
            .iter()
            .chain(deferred_non_mono_structs.iter())
            .cloned()
            .collect();
        let sorted_phase4 = Self::sort_types_topologically(&all_phase4_structs, &[], type_table);
        for type_decl in sorted_phase4 {
            let TypeDecl::Struct(tir_struct) = type_decl else {
                continue;
            };
            let struct_name = StructName::new(ModuleSource::entry_point(), tir_struct.name.clone());
            // Check for self-referential structs (e.g., BTreeNode with Array<&mut BTreeNode>)
            let self_ref_fields =
                Self::get_self_referential_field_types(&tir_struct.name, tir_struct, type_table);
            if self_ref_fields.is_empty() {
                self.register_struct_type(struct_name, tir_struct, type_table, &mut builder);
            } else {
                // Use rec group for self-referential structs
                self.register_self_referential_struct(
                    struct_name,
                    tir_struct,
                    &self_ref_fields,
                    type_table,
                    &mut builder,
                );
            }
        }

        // Register remaining tuple types that were skipped earlier due to unregistered generic instances.
        // Now that monomorphized structs are registered, these tuples can be created.
        self.register_tuple_types_from_table(type_table, &mut builder);
        for (module_source, tir_mod) in all_tir_modules {
            if module_source != entry_module_source {
                self.register_tuple_types_from_table(&tir_mod.type_table.borrow(), &mut builder);
            }
        }

        // PHASE 4.5: Register variant types (tagged unions) from imported modules
        // NOTE: Non-generic library variants are now registered in PHASE 1 together with structs
        //       using topological sorting to handle struct<->variant dependencies.

        // PHASE 4.5b: Register monomorphized generic variants from type tables
        // Scan for GenericInstance types that refer to variants and register them
        self.register_monomorphized_variants_from_table(entry_tir, type_table, &mut builder);
        for (module_source, tir_mod) in all_tir_modules {
            if module_source != entry_module_source {
                self.register_monomorphized_variants_from_table(
                    tir_mod,
                    &tir_mod.type_table.borrow(),
                    &mut builder,
                );
            }
        }

        // PHASE 4.6: Register Result types from type tables
        // Result<T, E> types are represented as variants with Ok and Err cases
        self.register_result_types_from_table(type_table, &mut builder);
        for (module_source, tir_mod) in all_tir_modules {
            if module_source != entry_module_source {
                self.register_result_types_from_table(&tir_mod.type_table.borrow(), &mut builder);
            }
        }

        // PHASE 5: Register ALL array types (including struct-based like Array<String>)
        // This must happen after ALL struct registration (including monomorphized ones)
        // because array types with struct elements need type_id_to_valtype to work.
        // Also must be after canonical closure types for Array<fn(...)> to work.
        self.register_array_types_from_table(type_table, &mut builder);
        for (module_source, tir_mod) in all_tir_modules {
            if module_source != entry_module_source {
                self.register_array_types_from_table(&tir_mod.type_table.borrow(), &mut builder);
            }
        }

        // WASI effect function types - derived from wasi/*.wado definitions
        // DCE: Only define types for WASI functions that are actually available (lowered)
        for interface in self.project.wasi_registry.interfaces() {
            for func in &interface.functions {
                let local_name = func.local_alias_name();
                // Only define type if this function is available (lowered at component level)
                if !available_wasi_funcs.contains(&local_name) {
                    continue;
                }
                let params = self.wasi_func_to_core_params(func);
                let results = self.wasi_func_to_core_results(func);
                builder.define_func_type(&local_name, &params, &results);
            }
        }

        // Types for user-defined functions from entry TIR module
        for tir_func_rc in &entry_tir.functions {
            let tir_func = tir_func_rc.borrow();
            // Skip functions that contain type parameters (from generic structs like Box<T>)
            // These functions need to be inlined at call sites with concrete types
            let has_type_params = type_table.contains_type_param(tir_func.return_type)
                || tir_func
                    .params
                    .iter()
                    .any(|p| type_table.contains_type_param(p.type_id));
            if has_type_params {
                continue;
            }
            let param_types: Vec<ValType> = tir_func
                .params
                .iter()
                .map(|p| self.type_id_to_valtype(type_table, p.type_id))
                .collect();

            // Never and Unit types have no Wasm return value
            let return_types: Vec<ValType> = if tir_func.return_type == TypeTable::NEVER
                || tir_func.return_type == TypeTable::UNIT
            {
                vec![]
            } else {
                vec![self.type_id_to_valtype(type_table, tir_func.return_type)]
            };

            // For methods, build the type name from method_info metadata
            let type_name = if let Some(ref info) = tir_func.method_info {
                MethodName::new(
                    entry_module_source.to_path().join("/"),
                    info.struct_name.clone(),
                    info.trait_name.clone(),
                    info.full_method_name(),
                )
                .to_string()
            } else {
                tir_func.name.clone()
            };
            builder.define_func_type(&type_name, &param_types, &return_types);
        }

        // Types for loaded module functions (TIR)
        for (_, tir_func_rc, func_type_table_rc, qualified_name) in &loaded_funcs {
            let tir_func = tir_func_rc.borrow();
            let func_type_table = &*func_type_table_rc.borrow();
            // Skip generic template functions - they will be registered when monomorphized
            if !tir_func.type_params.is_empty() || !tir_func.impl_type_params.is_empty() {
                // Generic template - skip unless it's a monomorphized instance
                if tir_func.monomorph_info.is_none() {
                    continue;
                }
            }

            let param_types: Vec<ValType> = tir_func
                .params
                .iter()
                .map(|p| self.type_id_to_valtype(func_type_table, p.type_id))
                .collect();
            let return_types: Vec<ValType> = if tir_func.return_type == TypeTable::NEVER
                || tir_func.return_type == TypeTable::UNIT
            {
                vec![]
            } else {
                vec![self.type_id_to_valtype(func_type_table, tir_func.return_type)]
            };
            builder.define_func_type(qualified_name, &param_types, &return_types);
        }

        // Types for impl methods from loaded modules (TIR)
        for (_module_path, struct_name, tir_method_rc, method_type_table_rc, mangled_name) in
            &loaded_methods
        {
            let tir_method = tir_method_rc.borrow();
            let method_type_table = &*method_type_table_rc.borrow();
            // Skip generic template methods - they will be registered when monomorphized
            if !tir_method.type_params.is_empty() || !tir_method.impl_type_params.is_empty() {
                // Generic template - skip unless it's a monomorphized instance
                if tir_method.monomorph_info.is_none() {
                    continue;
                }
            }

            let mut param_types: Vec<ValType> = Vec::new();

            for param in &tir_method.params {
                if param.name == "self" {
                    // &self parameter: use reference to struct type
                    if let Some(struct_info) = self.struct_types.get(struct_name) {
                        let struct_ref_type = ValType::Ref(RefType {
                            nullable: false,
                            heap_type: HeapType::Concrete(struct_info.type_idx),
                        });
                        param_types.push(struct_ref_type);
                    } else if struct_name.name.starts_with("Array<") {
                        // Handle monomorphized Array methods (Array<i32>, Array<String>, etc.)
                        // Use type_id_to_valtype which looks up Array types via struct_types
                        let self_valtype =
                            self.type_id_to_valtype(method_type_table, param.type_id);
                        // Convert to non-nullable reference for method call
                        let struct_ref_type = match self_valtype {
                            ValType::Ref(rt) => ValType::Ref(RefType {
                                nullable: false,
                                ..rt
                            }),
                            other => other,
                        };
                        param_types.push(struct_ref_type);
                    } else {
                        // For primitive type methods (i32, f64, etc.), use the type_id_to_valtype
                        // which handles reference types correctly (e.g., &i32 -> boxed i32 ref)
                        let self_valtype =
                            self.type_id_to_valtype(method_type_table, param.type_id);
                        param_types.push(self_valtype);
                    }
                } else {
                    param_types.push(self.type_id_to_valtype(method_type_table, param.type_id));
                }
            }

            let return_types: Vec<ValType> = if tir_method.return_type == TypeTable::NEVER
                || tir_method.return_type == TypeTable::UNIT
            {
                vec![]
            } else {
                vec![self.type_id_to_valtype(method_type_table, tir_method.return_type)]
            };
            builder.define_func_type(mangled_name, &param_types, &return_types);
        }

        // World export types - derived from target world (default: Command)
        // If a matching TIR function exists, use its params; otherwise use world definition
        if let Some(world_info) = self.project.world_registry.get(&project.target_world) {
            for export in &world_info.exports {
                // Check if there's a matching TIR function in the entry module
                let tir_func_match = entry_tir
                    .functions
                    .iter()
                    .find(|f| f.borrow().name == export.name);

                let (params, results) = if let Some(tir_func_rc) = tir_func_match {
                    // Use TIR function's actual signature
                    let tir_func = tir_func_rc.borrow();
                    let param_types: Vec<ValType> = tir_func
                        .params
                        .iter()
                        .map(|p| self.type_id_to_valtype(type_table, p.type_id))
                        .collect();
                    // Async exports have no return in core (use task_return)
                    // Never and Unit types also have no Wasm return value
                    let return_types = if export.is_async
                        || tir_func.return_type == TypeTable::NEVER
                        || tir_func.return_type == TypeTable::UNIT
                    {
                        vec![]
                    } else {
                        vec![self.type_id_to_valtype(type_table, tir_func.return_type)]
                    };
                    (param_types, return_types)
                } else {
                    // No matching TIR function - use world definition
                    let params = self.world_export_to_core_params(export);
                    let results = self.world_export_to_core_results(export);
                    (params, results)
                };
                builder.define_func_type(&export.name, &params, &results);
            }
        }

        // Add types section to module
        module.section(builder.types());

        // ========================================
        // Import section
        // ========================================
        // DCE: Only import builtins that are actually used (from TIR imports)
        for import in &entry_tir.imports {
            builder.import_func(&import.namespace, &import.canonical_name);
        }

        // Import lowered WASI functions
        // Only import functions that are available (lowered at component level)
        for local_name in available_wasi_funcs {
            builder.import_func("wasi", local_name);
        }

        // Import HTTP functions for Service world
        if self.project.has_http_handler_export {
            builder.import_func("wasi", "http-fields-constructor");
            builder.import_func("wasi", "http-response-new");
        }

        builder.import_memory("mem", "memory", 1);
        module.section(builder.imports());

        // ========================================
        // Function section
        // ========================================
        // Collect world export names to skip (they are handled as entry points)
        let world_export_names: HashSet<String> = project
            .world_registry
            .get(&project.target_world)
            .map(|w| w.exports.iter().map(|e| e.name.clone()).collect())
            .unwrap_or_else(|| std::iter::once("run".to_string()).collect());

        // Collect closure __call method indices for element segment (required for ref.func)
        let mut closure_call_func_indices: Vec<u32> = Vec::new();
        // Collect info for generating canonical wrappers
        let mut closure_call_wrapper_infos: Vec<ClosureCallWrapperInfo> = Vec::new();

        // Declare all TIR functions except world exports (which are handled as entry points)
        for tir_func_rc in &entry_tir.functions {
            let tir_func = tir_func_rc.borrow();
            if world_export_names.contains(&tir_func.name) {
                continue;
            }
            // Skip functions that contain type parameters (from generic structs like Box<T>)
            let has_type_params = type_table.contains_type_param(tir_func.return_type)
                || tir_func
                    .params
                    .iter()
                    .any(|p| type_table.contains_type_param(p.type_id));
            if has_type_params {
                continue;
            }
            // Methods have method_info metadata for building mangled names
            if let Some(ref info) = tir_func.method_info {
                let mangled_name = MethodName::new(
                    entry_module_source.to_path().join("/"),
                    info.struct_name.clone(),
                    info.trait_name.clone(),
                    info.full_method_name(),
                )
                .to_string();
                let func_idx = builder.define_func(&mangled_name, &mangled_name);
                // For monomorphized methods, also register an alias
                // with just the simple name (e.g., Array<i32>::len)
                if tir_func.monomorph_info.is_some() {
                    builder.define_func_alias(&tir_func.name, func_idx);
                }
                // For closure __call methods, collect info for canonical wrapper generation
                if info.struct_name.starts_with("__Closure_") {
                    builder.define_func_alias(&tir_func.name, func_idx);
                    // Track for element segment (required for ref.func)
                    closure_call_func_indices.push(func_idx);

                    // Extract functor_id from struct_name "__Closure_N"
                    if let Some(id_str) = info.struct_name.strip_prefix("__Closure_")
                        && let Ok(functor_id) = id_str.parse::<u32>()
                    {
                        // Get functor struct type index
                        let functor_type_idx = builder.type_idx(&info.struct_name);
                        // Get param types (skip first param which is self)
                        let param_type_ids: Vec<TypeId> =
                            tir_func.params.iter().skip(1).map(|p| p.type_id).collect();
                        closure_call_wrapper_infos.push(ClosureCallWrapperInfo {
                            functor_id,
                            call_func_idx: func_idx,
                            functor_type_idx,
                            param_type_ids,
                            return_type_id: tir_func.return_type,
                        });
                    }
                }
            } else {
                builder.define_func(&tir_func.name, &tir_func.name);
            }
        }
        // Declare loaded module functions with simple name aliases
        // This matches the AST path behavior where functions can be called by simple name
        let internal_source = ModuleSource::Core {
            name: "internal".to_string(),
        };
        for (module_source, tir_func_rc, _, qualified_name) in &loaded_funcs {
            let tir_func = tir_func_rc.borrow();
            let func_idx = builder.define_func(qualified_name, qualified_name);
            let is_from_internal = module_source == &internal_source;

            // Register simple name alias for all functions EXCEPT:
            // - Internal functions (require explicit import to be accessible)
            // - __initialize_module (each module has its own, called by qualified name)
            let is_init_module = tir_func.name == "__initialize_module";
            if qualified_name != &tir_func.name && !is_from_internal && !is_init_module {
                builder.define_func_alias(&tir_func.name, func_idx);
            }

            // Track internal functions for access control
            if is_from_internal {
                builder.mark_as_internal(&tir_func.name);
            }
        }
        // Declare impl methods from loaded modules
        for (_module_path, _struct_name, method_rc, _, mangled_name) in &loaded_methods {
            let method = method_rc.borrow();
            let func_idx = builder.define_func(mangled_name, mangled_name);
            // For monomorphized methods, also register an alias
            // with just the simple name (e.g., Array<String>::len)
            // This allows calling monomorphized methods without knowing which module they're in
            if method.monomorph_info.is_some() {
                builder.define_func_alias(&method.name, func_idx);
            }
        }
        // Declare world export functions (entry points)
        for export_name in &world_export_names {
            builder.define_func(export_name, export_name);
        }

        // Declare canonical wrapper functions for closure __call methods
        // These have canonical signature (ref struct, params...) -> result
        for wrapper_info in &closure_call_wrapper_infos {
            // Get canonical closure type for this signature
            let key = (
                wrapper_info.param_type_ids.clone(),
                wrapper_info.return_type_id,
            );
            if let Some((_, fn_type_name, _)) = self.canonical_closure_types.get(&key) {
                let wrapper_name = format!("__Closure_{}_canonical", wrapper_info.functor_id);
                let wrapper_func_idx = builder.define_func(&wrapper_name, fn_type_name);
                // Track wrapper for use in ClosureToCanonical
                self.closure_canonical_wrappers
                    .insert(wrapper_info.functor_id, wrapper_func_idx);
                // Also add to element segment for ref.func
                closure_call_func_indices.push(wrapper_func_idx);
            }
        }

        module.section(builder.functions());

        // ========================================
        // Global section
        // ========================================
        // Define user-defined global variables from all modules
        for (module_source, tir_mod) in all_tir_modules {
            let mod_type_table = &*tir_mod.type_table.borrow();
            for global in &tir_mod.globals {
                let mut val_type = self.type_id_to_valtype(mod_type_table, global.ty);
                // For nullable globals (lazy-init reference types), make the Wasm type nullable
                if global.is_nullable
                    && let ValType::Ref(ref_type) = val_type
                {
                    val_type = ValType::Ref(RefType {
                        nullable: true,
                        ..ref_type
                    });
                }
                // Get constant expression for initializer
                let init_expr =
                    Self::global_init_to_const_expr(&global.initializer, mod_type_table);
                // Create qualified name for the global (matching the lookup pattern)
                let global_name = if module_source == entry_module_source {
                    format!("global:{}", global.name)
                } else {
                    let module_path = module_source.to_path();
                    format!("global:{}::{}", module_path.join("::"), global.name)
                };
                // mutable is already set by lower phase (includes lazy-init globals)
                builder.define_global(
                    &global_name,
                    val_type,
                    global.mutable,
                    init_expr,
                    global.is_nullable,
                );
            }
        }
        // Add globals section only if there are globals
        if builder.has_globals() {
            module.section(builder.globals());
        }

        // ========================================
        // Export section
        // ========================================
        // Export world functions based on target world
        if let Some(world_info) = self.project.world_registry.get(&project.target_world) {
            for export in &world_info.exports {
                builder.export_func(&export.name, &export.name);
            }
        } else {
            // Fallback to "run" for unknown worlds
            builder.export_func("run", "run");
        }
        // Export test functions for test runner
        for test in &entry_tir.tests {
            builder.export_func(&test.function_name, &test.function_name);
        }
        module.section(builder.exports());

        // ========================================
        // Element section (required for ref.func in closures)
        // ========================================
        if !closure_call_func_indices.is_empty() {
            let mut elements = ElementSection::new();
            // Create declarative element segment for ref.func usage
            elements.declared(Elements::Functions(std::borrow::Cow::Borrowed(
                &closure_call_func_indices,
            )));
            module.section(&elements);
        }

        // Data count section (required for array.new_data with GC)
        let data_count = u32::from(!string_data.is_empty());
        module.section(&DataCountSection { count: data_count });

        // ========================================
        // Code section
        // ========================================
        let mut code = CodeSection::new();
        let mut all_branch_hints: Vec<(u32, Vec<(u32, bool)>)> = Vec::new();
        let mut func_idx = builder.import_func_count;
        let empty_path: &[String] = &[];

        // Generate user-defined functions from entry TIR (excluding world exports which are handled specially)
        for tir_func_rc in &entry_tir.functions {
            let tir_func = tir_func_rc.borrow();
            if world_export_names.contains(&tir_func.name) {
                continue; // Skip world exports - they are handled separately as entry points
            }
            // Skip functions that contain type parameters (from generic structs like Box<T>)
            let has_type_params = type_table.contains_type_param(tir_func.return_type)
                || tir_func
                    .params
                    .iter()
                    .any(|p| type_table.contains_type_param(p.type_id));
            if has_type_params {
                continue;
            }
            // Test functions need task.return wrapper like run
            if tir_func.name.starts_with("__test_") {
                let wasm_func = self.generate_run_function(&tir_func, type_table, &builder);
                code.function(&wasm_func);
            } else {
                let (wasm_func, hints) =
                    self.generate_function(&tir_func, type_table, &builder, empty_path);
                code.function(&wasm_func);
                if !hints.is_empty() {
                    all_branch_hints.push((func_idx, hints));
                }
            }
            func_idx += 1;
        }

        // Generate loaded module functions (TIR path)
        for (module_source, tir_func_rc, func_type_table_rc, _qualified_name) in &loaded_funcs {
            let tir_func = tir_func_rc.borrow();
            let func_type_table = &*func_type_table_rc.borrow();
            // Skip generic template functions - only generate monomorphized instances
            if (!tir_func.type_params.is_empty() || !tir_func.impl_type_params.is_empty())
                && tir_func.monomorph_info.is_none()
            {
                continue;
            }

            let module_path = module_source.to_path();
            let (wasm_func, hints) =
                self.generate_function(&tir_func, func_type_table, &builder, &module_path);
            code.function(&wasm_func);
            if !hints.is_empty() {
                all_branch_hints.push((func_idx, hints));
            }
            func_idx += 1;
        }

        // Generate impl methods from loaded modules (TIR path)
        for (module_source, _struct_name, tir_method_rc, method_type_table_rc, _mangled_name) in
            &loaded_methods
        {
            let tir_method = tir_method_rc.borrow();
            let method_type_table = &*method_type_table_rc.borrow();
            // Skip generic template methods - only generate monomorphized instances
            if (!tir_method.type_params.is_empty() || !tir_method.impl_type_params.is_empty())
                && tir_method.monomorph_info.is_none()
            {
                continue;
            }

            let module_path = module_source.to_path();
            let (wasm_func, hints) =
                self.generate_function(&tir_method, method_type_table, &builder, &module_path);
            code.function(&wasm_func);
            if !hints.is_empty() {
                all_branch_hints.push((func_idx, hints));
            }
            func_idx += 1;
        }

        // Generate world export functions (entry points with task.return wrapper)
        for export_name in &world_export_names {
            let export_tir_rc = entry_tir
                .functions
                .iter()
                .find(|f| f.borrow().name == *export_name);

            let export_wasm_func = if let Some(tir_rc) = export_tir_rc {
                // Generate function body using the TIR function body generation
                let tir_func = tir_rc.borrow();
                self.generate_run_function(&tir_func, type_table, &builder)
            } else {
                // No matching function - create empty entry point
                let mut func = Function::new(vec![]);
                let task_return_idx = builder.func_idx("task-return");
                if self.project.has_http_handler_export {
                    // Service world: result<own<response>, error-code> with complex payloads
                    // Flattens to: (i32, i32, i32, i64, i32, i32, i32, i32)
                    func.instruction(&Instruction::I32Const(1)); // Err discriminant
                    func.instruction(&Instruction::I32Const(38)); // internal-error
                    func.instruction(&Instruction::I32Const(1)); // option<string> has Some
                    func.instruction(&Instruction::I64Const(0)); // u64 padding
                    func.instruction(&Instruction::I32Const(0)); // string ptr
                    func.instruction(&Instruction::I32Const(37)); // string len
                    func.instruction(&Instruction::I32Const(0)); // padding
                    func.instruction(&Instruction::I32Const(0)); // padding
                } else {
                    // Command world: result<_, _> needs just (i32)
                    func.instruction(&Instruction::I32Const(0)); // Ok discriminant
                }
                func.instruction(&Instruction::Call(task_return_idx));
                func.instruction(&Instruction::End);
                func
            };

            code.function(&export_wasm_func);
        }

        // Generate canonical wrapper function bodies for closure __call methods
        // These cast (ref struct) to the specific functor type and call the original __call
        for wrapper_info in &closure_call_wrapper_infos {
            // Skip if wrapper wasn't defined (canonical type not registered)
            if !self
                .closure_canonical_wrappers
                .contains_key(&wrapper_info.functor_id)
            {
                continue;
            }

            let mut wrapper_func = Function::new(vec![]);

            // Cast first param (ref struct) to specific functor type
            wrapper_func.instruction(&Instruction::LocalGet(0)); // env param
            wrapper_func.instruction(&Instruction::RefCastNonNull(HeapType::Concrete(
                wrapper_info.functor_type_idx,
            )));

            // Pass through all other params
            for i in 0..wrapper_info.param_type_ids.len() {
                wrapper_func.instruction(&Instruction::LocalGet((i + 1) as u32));
            }

            // Call the original __call method
            wrapper_func.instruction(&Instruction::Call(wrapper_info.call_func_idx));

            wrapper_func.instruction(&Instruction::End);
            code.function(&wrapper_func);
        }

        // Branch hints section (emit before code section for proper placement)
        if !all_branch_hints.is_empty() {
            let mut hints = BranchHints::new();
            for (func_idx, func_hints) in all_branch_hints {
                hints.function_hints(
                    func_idx,
                    func_hints.into_iter().map(|(offset, taken)| BranchHint {
                        branch_func_offset: offset,
                        branch_hint_value: u32::from(taken),
                    }),
                );
            }
            module.section(&hints);
        }

        module.section(&code);

        // Data section
        if !string_data.is_empty() {
            let mut data = DataSection::new();
            data.passive(string_data.iter().copied());
            module.section(&data);
        }

        // Name section (skip in size-optimized builds)
        if !strip_names {
            let names = builder.build_name_section(module_name);
            module.section(&names);
        }

        // Producers section (skip in size-optimized builds)
        if !strip_names {
            let producers = CoreModuleBuilder::build_producers_section();
            module.section(&producers);
        }

        module.finish()
    }

    /// Generate component from TIR for WASI P3
    /// Uses native stream<T> types and imports wasi:cli/stdout
    fn generate_component(&mut self) -> Vec<u8> {
        let project = self.project;
        let entry_tir = project.entry_module();
        let all_tir_modules = &project.tir_modules;
        let symbols = &project.symbols;
        let module_name = &project.module_name;

        let mut builder = ComponentBuilder::default();
        let mut ctx = ComponentModelContext::new();

        // Build string data for memory
        let string_data: Vec<u8> = self
            .string_literals
            .iter()
            .flat_map(|s| s.bytes())
            .collect();

        // ========================================
        // Generate WASI imports dynamically from registry
        // (same as AST path - imports all supported interfaces)
        // ========================================
        self.generate_wasi_imports(&mut builder, &mut ctx, project);

        // ========================================
        // Type: stream<u8> for stream intrinsics
        // ========================================
        let stream_u8_type = ctx.register_type("stream-u8");
        {
            let (_, enc) = builder.ty(Some("stream-u8"));
            enc.defined_type()
                .stream(Some(ComponentValType::Primitive(PrimitiveValType::U8)));
        }

        // ========================================
        // Type: result unit for run function (needed for task.return)
        // ========================================
        let result_unit_type = ctx.register_type("result-unit");
        {
            let (_, enc) = builder.ty(Some("result-unit"));
            enc.defined_type().result(None, None);
        }

        // ========================================
        // Core memory module
        // ========================================
        let mem_module = self.build_memory_module(project.strip_names);
        ctx.register_core_module("mem-mod");
        builder.core_module_raw(Some("mem-mod"), &mem_module);

        // Instantiate memory module
        ctx.register_core_instance("mem");
        builder.core_instantiate(
            Some("mem"),
            ctx.core_module_idx("mem-mod"),
            Vec::<(&str, ModuleArg)>::new(),
        );

        // Alias memory and realloc from mem instance
        ctx.set_memory(0); // memory is always index 0 at core level
        builder.core_alias_export(
            Some("memory"),
            ctx.core_instance_idx("mem"),
            "memory",
            ExportKind::Memory,
        );
        ctx.register_core_func("realloc");
        builder.core_alias_export(
            Some("realloc"),
            ctx.core_instance_idx("mem"),
            "realloc",
            ExportKind::Func,
        );

        // ========================================
        // Wado-bundled module (float-to-string and libm, conditionally included)
        // ========================================
        // Check if we need bundled module (float-to-string or libm)
        // Bundled imports have namespace "bundled"
        let bundled_imports: Vec<&TirImport> = entry_tir
            .imports
            .iter()
            .filter(|i| i.namespace == "bundled")
            .collect();
        if !bundled_imports.is_empty() {
            // Convert memory to import
            let bundled_module =
                wasm_postprocess::convert_memory_to_import(wado_bundled_wasm(), "env", "memory")
                    .expect("Failed to process wado-bundled module");

            // Apply Wasm-level DCE if enabled (disabled for -O0)
            let final_module = if project.wasm_dce_enabled {
                // Build set of exports to keep based on used bundled imports
                let keep_exports: std::collections::HashSet<_> = bundled_imports
                    .iter()
                    .map(|i| i.canonical_name.clone())
                    .collect();
                wasm_postprocess::eliminate_dead_code(&bundled_module, &keep_exports)
            } else {
                bundled_module
            };

            ctx.register_core_module("fts-mod");
            builder.core_module_raw(Some("fts-mod"), &final_module);

            // Create env instance for bundled module (just memory)
            ctx.register_core_instance("fts-env");
            let fts_env_exports = [("memory", ExportKind::Memory, ctx.memory_idx())];
            let fts_env_instance =
                builder.core_instantiate_exports(Some("fts-env-instance"), fts_env_exports);

            // Instantiate bundled module with memory
            ctx.register_core_instance("fts");
            builder.core_instantiate(
                Some("fts"),
                ctx.core_module_idx("fts-mod"),
                [("env", ModuleArg::Instance(fts_env_instance))],
            );

            // Alias bundled exports (float-to-string and libm)
            for import in &bundled_imports {
                ctx.register_core_func(&import.canonical_name);
                builder.core_alias_export(
                    Some(&import.canonical_name),
                    ctx.core_instance_idx("fts"),
                    &import.canonical_name,
                    ExportKind::Func,
                );
            }
        }

        // ========================================
        // HTTP response types for future<T> canonical intrinsics
        // Only defined if any future-* canonical intrinsics are used
        // (DCE determines this from reachability analysis starting from world exports)
        // ========================================
        let needs_future_intrinsics = entry_tir.imports.iter().any(|i| {
            i.namespace == "wasi"
                && matches!(
                    i.canonical_name.as_str(),
                    "future-new" | "future-write" | "future-drop-writable" | "future-drop-readable"
                )
        });

        let trailers_future_type = if needs_future_intrinsics {
            // Define own<fields> type for use in option<fields> (trailers)
            // This must be an owned handle type, not just u32, for type compatibility
            // with response.new's trailers parameter
            ctx.register_type("http-fields");
            {
                let fields_resource_idx = ctx.type_idx("http-fields-resource");
                let (_, enc) = builder.ty(Some("http-fields"));
                enc.defined_type().own(fields_resource_idx);
            }

            // Define option<stream<u8>> type for body
            ctx.register_type("http-option-stream-u8");
            {
                let (_, enc) = builder.ty(Some("http-option-stream-u8"));
                enc.defined_type()
                    .option(ComponentValType::Type(stream_u8_type));
            }

            // Define option<fields> for trailers
            ctx.register_type("http-option-fields");
            {
                let fields_idx = ctx.type_idx("http-fields");
                let (_, enc) = builder.ty(Some("http-option-fields"));
                enc.defined_type()
                    .option(ComponentValType::Type(fields_idx));
            }

            // Define result<option<fields>, error-code> for trailers future payload
            ctx.register_type("http-trailers-result");
            {
                let option_fields_idx = ctx.type_idx("http-option-fields");
                let error_code_idx = ctx.type_idx("http-error-code");
                let (_, enc) = builder.ty(Some("http-trailers-result"));
                enc.defined_type().result(
                    Some(ComponentValType::Type(option_fields_idx)),
                    Some(ComponentValType::Type(error_code_idx)),
                );
            }

            // Define future<result<option<fields>, error-code>> for trailers
            let trailers_future_type = ctx.register_type("http-trailers-future");
            {
                let trailers_result_idx = ctx.type_idx("http-trailers-result");
                let (_, enc) = builder.ty(Some("http-trailers-future"));
                enc.defined_type()
                    .future(Some(ComponentValType::Type(trailers_result_idx)));
            }

            // Define result<_, error-code> for transmission future payload
            ctx.register_type("http-transmission-result");
            {
                let error_code_idx = ctx.type_idx("http-error-code");
                let (_, enc) = builder.ty(Some("http-transmission-result"));
                enc.defined_type()
                    .result(None, Some(ComponentValType::Type(error_code_idx)));
            }

            // Define future<result<_, error-code>> for transmission future
            ctx.register_type("http-transmission-future");
            {
                let transmission_result_idx = ctx.type_idx("http-transmission-result");
                let (_, enc) = builder.ty(Some("http-transmission-future"));
                enc.defined_type()
                    .future(Some(ComponentValType::Type(transmission_result_idx)));
            }

            trailers_future_type
        } else {
            0 // Placeholder - not used when future intrinsics aren't needed
        };

        // ========================================
        // Canonical intrinsics - emit based on TIR imports with "wasi" namespace
        // Each import is emitted exactly once, driven by DCE reachability
        // ========================================
        for import in entry_tir.imports.iter().filter(|i| i.namespace == "wasi") {
            let name = import.canonical_name.as_str();
            ctx.register_core_func(name);

            match name {
                // Stream intrinsics (stream<u8>)
                "stream-new" => {
                    builder.stream_new(stream_u8_type);
                }
                "stream-write" => {
                    builder.stream_write(
                        stream_u8_type,
                        [
                            CanonicalOption::Memory(ctx.memory_idx()),
                            CanonicalOption::Realloc(ctx.core_func_idx("realloc")),
                        ],
                    );
                }
                "stream-drop-writable" => {
                    builder.stream_drop_writable(stream_u8_type);
                }
                "stream-drop-readable" => {
                    builder.stream_drop_readable(stream_u8_type);
                }

                // Future intrinsics (future<trailers>)
                "future-new" => {
                    builder.future_new(trailers_future_type);
                }
                "future-write" => {
                    builder.future_write(
                        trailers_future_type,
                        [
                            CanonicalOption::Memory(ctx.memory_idx()),
                            CanonicalOption::Realloc(ctx.core_func_idx("realloc")),
                        ],
                    );
                }
                "future-drop-writable" => {
                    builder.future_drop_writable(trailers_future_type);
                }
                "future-drop-readable" => {
                    builder.future_drop_readable(trailers_future_type);
                }

                // Async task intrinsics
                "task-return" => {
                    // task-return type depends on whether http-handler-result is defined
                    let task_return_type = if ctx.has_type("http-handler-result") {
                        ctx.type_idx("http-handler-result")
                    } else {
                        result_unit_type
                    };
                    builder.task_return(
                        Some(ComponentValType::Type(task_return_type)),
                        [CanonicalOption::Memory(ctx.memory_idx())],
                    );
                }
                "waitable-set-new" => {
                    builder.waitable_set_new();
                }
                "waitable-join" => {
                    builder.waitable_join();
                }
                "waitable-set-wait" => {
                    builder.waitable_set_wait(false, ctx.memory_idx());
                }
                "subtask-drop" => {
                    builder.subtask_drop();
                }

                // Unknown canonical intrinsic - skip (might be handled elsewhere)
                _ => {}
            }
        }

        // ========================================
        // Lower all WASI functions using registry data
        // Canonical options are derived from CmCallConvention
        // ========================================
        self.lower_wasi_functions(&mut builder, &mut ctx);

        // ========================================
        // Lower HTTP types functions (if component functions are available)
        // ========================================
        if ctx.has_comp_func("http-fields-constructor") {
            // Lower [constructor]fields: () -> own<fields>
            // Constructor returns a resource handle (i32)
            ctx.register_core_func("http-fields-constructor");
            builder.lower_func(
                Some("http-fields-constructor"),
                ctx.comp_func_idx("http-fields-constructor"),
                [],
            );

            // Lower [static]response.new:
            // (own<fields>, option<stream<u8>>, future<...>) -> result<response, transmission-result>
            // Needs memory and realloc for complex types
            ctx.register_core_func("http-response-new");
            builder.lower_func(
                Some("http-response-new"),
                ctx.comp_func_idx("http-response-new"),
                [
                    CanonicalOption::Memory(ctx.memory_idx()),
                    CanonicalOption::Realloc(ctx.core_func_idx("realloc")),
                ],
            );
        }

        // ========================================
        // Collect available WASI functions (those that were lowered)
        // ========================================
        let mut available_wasi_funcs: HashSet<String> = HashSet::new();
        for interface in self.project.wasi_registry.interfaces() {
            for func in &interface.functions {
                let local_name = func.local_alias_name();
                if ctx.has_core_func(&local_name) {
                    available_wasi_funcs.insert(local_name);
                }
            }
        }

        // ========================================
        // Main core module
        // ========================================
        let main_module = self.build_main_module(BuildMainModuleParams {
            entry_tir,
            all_tir_modules,
            symbols,
            string_data: &string_data,
            project,
            module_name,
            available_wasi_funcs: &available_wasi_funcs,
        });
        // Validate main module before embedding
        {
            let mut validator = Validator::new_with_features(WasmFeatures::all());
            if let Err(e) = validator.validate_all(&main_module) {
                // Print WAT for debugging before panicking
                eprintln!("=== MAIN MODULE WAT (for debugging) ===");
                if let Ok(wat) = wasmprinter::print_bytes(&main_module) {
                    eprintln!("{wat}");
                }
                eprintln!("=== END MAIN MODULE WAT ===");
                panic!("Core module validation failed: {e}");
            }
        }
        ctx.register_core_module("main-mod");
        builder.core_module_raw(Some("main-mod"), &main_module);

        // Create wasi instance with canon intrinsics + lowered WASI functions
        // (env intrinsics are handled separately in env-instance)
        let mut wasi_exports: Vec<(String, ExportKind, u32)> = Vec::new();

        // Add canonical builtins with namespace "wasi" (from TIR imports)
        for import in &entry_tir.imports {
            if import.namespace == "wasi" {
                wasi_exports.push((
                    import.canonical_name.clone(),
                    ExportKind::Func,
                    ctx.core_func_idx(&import.canonical_name),
                ));
            }
        }

        // Add lowered WASI functions (Stdout::write_via_stream, etc.)
        for local_name in &available_wasi_funcs {
            wasi_exports.push((
                local_name.clone(),
                ExportKind::Func,
                ctx.core_func_idx(local_name),
            ));
        }

        // Add lowered HTTP types functions for Service world
        if self.project.has_http_handler_export && ctx.has_core_func("http-fields-constructor") {
            wasi_exports.push((
                "http-fields-constructor".to_string(),
                ExportKind::Func,
                ctx.core_func_idx("http-fields-constructor"),
            ));
            wasi_exports.push((
                "http-response-new".to_string(),
                ExportKind::Func,
                ctx.core_func_idx("http-response-new"),
            ));
        }

        let wasi_exports_refs: Vec<_> = wasi_exports
            .iter()
            .map(|(name, kind, idx)| (name.as_str(), *kind, *idx))
            .collect();
        let wasi_instance =
            builder.core_instantiate_exports(Some("wasi-instance"), wasi_exports_refs);
        ctx.register_core_instance("wasi");

        // Build "mem" instance with memory + realloc
        let mem_exports: Vec<(&str, ExportKind, u32)> = vec![
            ("memory", ExportKind::Memory, ctx.memory_idx()),
            ("realloc", ExportKind::Func, ctx.core_func_idx("realloc")),
        ];
        let mem_instance = builder.core_instantiate_exports(Some("mem-instance"), mem_exports);
        ctx.register_core_instance("mem");

        // Build "bundled" instance with bundled exports (if any)
        let bundled_exports: Vec<(String, ExportKind, u32)> = entry_tir
            .imports
            .iter()
            .filter(|i| i.namespace == "bundled")
            .map(|import| {
                (
                    import.canonical_name.clone(),
                    ExportKind::Func,
                    ctx.core_func_idx(&import.canonical_name),
                )
            })
            .collect();

        let bundled_instance = if bundled_exports.is_empty() {
            None
        } else {
            let bundled_exports_refs: Vec<_> = bundled_exports
                .iter()
                .map(|(name, kind, idx)| (name.as_str(), *kind, *idx))
                .collect();
            let instance =
                builder.core_instantiate_exports(Some("bundled-instance"), bundled_exports_refs);
            ctx.register_core_instance("bundled");
            Some(instance)
        };

        // Instantiate main module with wasi, mem, and optionally bundled instances
        ctx.register_core_instance("main");
        let mut main_args: Vec<(&str, ModuleArg)> = vec![
            ("wasi", ModuleArg::Instance(wasi_instance)),
            ("mem", ModuleArg::Instance(mem_instance)),
        ];
        if let Some(bundled_inst) = bundled_instance {
            main_args.push(("bundled", ModuleArg::Instance(bundled_inst)));
        }
        builder.core_instantiate(Some("main"), ctx.core_module_idx("main-mod"), main_args);

        // Export world functions based on target world
        let world_exports: Vec<_> = project
            .world_registry
            .get(&project.target_world)
            .map(|w| w.exports.clone())
            .unwrap_or_else(|| {
                // Fallback to a default run export for unknown worlds
                vec![crate::world_registry::WorldExportInfo {
                    name: "run".to_string(),
                    is_async: true,
                    params: vec![],
                    return_type: None,
                }]
            });

        for export in &world_exports {
            let core_name = format!("{}-core", export.name);
            let func_type_name = format!("{}-func-type", export.name);

            // Alias function from main instance
            ctx.register_core_func(&core_name);
            builder.core_alias_export(
                Some(&core_name),
                ctx.core_instance_idx("main"),
                &export.name,
                ExportKind::Func,
            );

            // Type: async function type with appropriate params and result
            // For Service world handle function, use Request param type
            let func_type = ctx.register_type(&func_type_name);
            {
                let (_, enc) = builder.ty(Some(&func_type_name));

                // Check if this is the Service world's handle function with Request param
                let is_service_handle = self.project.has_http_handler_export
                    && export.name == "handle"
                    && !export.params.is_empty();

                if is_service_handle {
                    // Use the imported http-request type for Service world handle function
                    // The param is own<request> which is lowered to i32 by canon lift
                    let request_type_idx = ctx.type_idx("http-request");
                    // Use result<response, error-code> for the return type
                    let handler_result_type_idx = ctx.type_idx("http-handler-result");
                    enc.function()
                        .async_(export.is_async)
                        .params([("request", ComponentValType::Type(request_type_idx))])
                        .result(Some(ComponentValType::Type(handler_result_type_idx)));
                } else {
                    // Default: no params, result<_, _>
                    enc.function()
                        .async_(export.is_async)
                        .params::<[(&str, ComponentValType); 0], ComponentValType>([])
                        .result(Some(ComponentValType::Type(result_unit_type)));
                }
            }

            // Lift function with Async option
            ctx.register_comp_func(&export.name);
            builder.lift_func(
                Some(&export.name),
                ctx.core_func_idx(&core_name),
                func_type,
                [
                    CanonicalOption::Async,
                    CanonicalOption::Memory(ctx.memory_idx()),
                ],
            );

            // Export function
            builder.export(
                &export.name,
                ComponentExportKind::Func,
                ctx.comp_func_idx(&export.name),
                None,
            );
            // Export consumes a component function index
            ctx.skip_comp_func_idx();
        }

        // Export test functions
        for test in &entry_tir.tests {
            // Convert __test_0_simple to test-0-simple for component export (kebab-case)
            let export_name = test.function_name.trim_start_matches('_').replace('_', "-");
            let core_name = format!("{export_name}-core");
            let test_func_type_name = format!("{export_name}-func-type");

            // Alias test function from main instance
            ctx.register_core_func(&core_name);
            builder.core_alias_export(
                Some(&core_name),
                ctx.core_instance_idx("main"),
                &test.function_name,
                ExportKind::Func,
            );

            // Type: async test function type () -> result<_, _>
            let test_func_type = ctx.register_type(&test_func_type_name);
            {
                let (_, enc) = builder.ty(Some(&test_func_type_name));
                enc.function()
                    .async_(true)
                    .params::<[(&str, ComponentValType); 0], ComponentValType>([])
                    .result(Some(ComponentValType::Type(result_unit_type)));
            }

            // Lift test function with Async option
            ctx.register_comp_func(&export_name);
            builder.lift_func(
                Some(&export_name),
                ctx.core_func_idx(&core_name),
                test_func_type,
                [
                    CanonicalOption::Async,
                    CanonicalOption::Memory(ctx.memory_idx()),
                ],
            );

            // Export test function
            builder.export(
                &export_name,
                ComponentExportKind::Func,
                ctx.comp_func_idx(&export_name),
                None,
            );
            // Export consumes a component function index
            ctx.skip_comp_func_idx();
        }

        // Add component-level debug names (skip in size-optimized builds)
        if !project.strip_names {
            builder.append_names();
        }

        let mut component_bytes = builder.finish();

        // For Service world, add the handler interface export
        // This creates a component instance containing the handle function
        // and exports it as wasi:http/handler interface
        if self.project.has_http_handler_export {
            Self::append_http_handler_export(&mut component_bytes, &ctx);
        }

        component_bytes
    }

    /// Append HTTP handler interface export to component bytes.
    ///
    /// This adds a component instance containing the handle function
    /// and exports it as `wasi:http/handler@0.3.0-rc-2026-01-06`.
    ///
    /// We do this as post-processing because `ComponentBuilder` doesn't expose
    /// a public method to create component instances from exports.
    fn append_http_handler_export(component_bytes: &mut Vec<u8>, ctx: &ComponentModelContext) {
        use wasm_encoder::{ComponentExportSection, ComponentInstanceSection, ComponentSection};

        // Get the indices of the handle function and types
        let handle_func_idx = ctx.comp_func_idx("handle");

        // Get the type indices (aliased from http-types import)
        let request_type_idx = ctx.type_idx("http-request-resource");
        let response_type_idx = ctx.type_idx("http-response-resource");
        let error_code_type_idx = ctx.type_idx("http-error-code");

        // Create a component instance with the handler interface exports
        // The interface uses: `use types.{request, response, error-code}`
        // and exports: `handle: async func(request: request) -> result<response, error-code>`
        let mut instances = ComponentInstanceSection::new();
        instances.export_items([
            ("request", ComponentExportKind::Type, request_type_idx),
            ("response", ComponentExportKind::Type, response_type_idx),
            ("error-code", ComponentExportKind::Type, error_code_type_idx),
            ("handle", ComponentExportKind::Func, handle_func_idx),
        ]);

        // The instance index will be the next available (current count)
        let instance_idx = ctx.instance_count();

        // Create an export for the handler interface
        let mut exports = ComponentExportSection::new();
        let http_version = "0.3.0-rc-2026-01-06";
        let handler_path = format!("wasi:http/handler@{http_version}");
        exports.export(
            &handler_path,
            ComponentExportKind::Instance,
            instance_idx,
            None,
        );

        // Append the instance and export sections to the component
        instances.append_to_component(component_bytes);
        exports.append_to_component(component_bytes);
    }

    /// Generate WASI imports dynamically from the registry
    ///
    /// This generates Component Model imports based on the WASI registry data
    /// populated from lib/wasi/*.wado files.
    fn generate_wasi_imports(
        &self,
        builder: &mut ComponentBuilder,
        ctx: &mut ComponentModelContext,
        project: &Project,
    ) {
        // Get the CLI version from the registry
        let cli_version = project
            .wasi_registry
            .get_cli_version()
            .expect("WASI CLI version not found in registry - lib/wasi/*.wado not loaded?");

        // First, import wasi:cli/types for shared types (error-code)
        // This must come first as other interfaces reference error-code
        let types_instance_type = ctx.register_type("types-instance-type");
        {
            let (_, enc) = builder.ty(Some("types-instance-type"));
            let mut instance_type = InstanceType::new();
            instance_type
                .ty()
                .defined_type()
                .enum_type(["io", "illegal-byte-sequence", "pipe"]);
            instance_type.export(
                "error-code",
                wasm_encoder::ComponentTypeRef::Type(TypeBounds::Eq(0)),
            );
            enc.instance(&instance_type);
        }

        ctx.register_instance("types");
        let types_import_path = format!("wasi:cli/types@{cli_version}");
        builder.import(
            &types_import_path,
            wasm_encoder::ComponentTypeRef::Instance(types_instance_type),
        );

        ctx.register_type("error-code");
        builder.alias_export(
            ctx.instance_idx("types"),
            "error-code",
            ComponentExportKind::Type,
        );

        // Now generate imports for each interface in the registry
        // Dynamically filter based on whether function types are supported
        for interface_info in self.project.wasi_registry.interfaces() {
            // Skip interfaces that define exports (not imports)
            // The "run" interface defines the component's entry point export.
            // Note: "run" is needed for the wasi:cli Command world, which Wado
            // doesn't fully implement yet. When Command world support is added,
            // this should be handled as an export, not an import.
            if interface_info.interface == "run" {
                continue;
            }

            // Skip interfaces that have resource types - these are handled separately
            // by import_interfaces_with_resources
            if interface_info.resource_type.is_some() {
                continue;
            }

            // Filter to only include functions with supported types
            // This allows importing a subset of functions from an interface
            // when some functions use unsupported types (like variants)
            let supported_functions: Vec<_> = interface_info
                .functions
                .iter()
                .filter(|func| {
                    // Check if function has supported types
                    if !self.project.wasi_registry.is_function_supported(func) {
                        return false;
                    }
                    // DCE: Only include functions that are actually used
                    let effect_name = &func.effect_name;
                    project.has_effect(effect_name)
                })
                .collect();

            // Skip interface if no functions are supported and used
            if supported_functions.is_empty() {
                continue;
            }

            // Build instance type for this interface
            let instance_type_name = format!("{}-instance-type", interface_info.interface);
            let instance_type_idx = ctx.register_type(&instance_type_name);
            {
                let (_, enc) = builder.ty(Some(&instance_type_name));
                let mut instance_type = InstanceType::new();
                let mut local_type_idx = 0u32;

                // FIRST: Collect resource types needed by static methods
                // Static methods return Result<Resource, ErrorCode> where Resource is an own<resource>
                // Resource types MUST come first in the instance type
                let mut needed_resources: Vec<String> = Vec::new();
                for func in &supported_functions {
                    // Check if this is a static method by looking at the function name
                    if func.wasi_func_name.starts_with("[static]") {
                        // Extract resource from return type (e.g., Result<TcpSocket, ErrorCode>)
                        if let Some(Type::Generic(g)) = &func.return_type
                            && g.name == "Result"
                            && !g.args.is_empty()
                            && let Type::Named(named) = &g.args[0]
                            && self.project.wasi_registry.is_resource(&named.name)
                            && !needed_resources.contains(&named.name)
                        {
                            needed_resources.push(named.name.clone());
                        }
                    }
                }

                // Define resource types and track their indices
                // For each resource, we define:
                //   - The resource type itself (SubResource for imports)
                //   - own<resource> type
                let mut resource_type_indices: HashMap<String, u32> = HashMap::new();
                let mut own_resource_type_indices: HashMap<String, u32> = HashMap::new();
                for resource_name in &needed_resources {
                    if let Some(cm_name) = self
                        .project
                        .wasi_registry
                        .get_resource_cm_name(resource_name)
                    {
                        // Export resource type (SubResource for imported resources)
                        instance_type.export(
                            cm_name,
                            wasm_encoder::ComponentTypeRef::Type(TypeBounds::SubResource),
                        );
                        resource_type_indices.insert(resource_name.clone(), local_type_idx);
                        local_type_idx += 1;

                        // Define own<resource> type referencing the resource
                        let resource_idx = resource_type_indices[resource_name];
                        instance_type.ty().defined_type().own(resource_idx);
                        own_resource_type_indices.insert(resource_name.clone(), local_type_idx);
                        local_type_idx += 1;
                    }
                }

                // SECOND: Collect all unique enum types needed by functions in this interface
                let mut needed_enums: Vec<String> = Vec::new();
                for func in &supported_functions {
                    for (_, ty) in &func.params {
                        if let Type::Named(named) = ty
                            && self.project.wasi_registry.is_enum(&named.name)
                            && !needed_enums.contains(&named.name)
                        {
                            needed_enums.push(named.name.clone());
                        }
                    }
                    // Also check return types for enums (e.g., Result<Resource, ErrorCode>)
                    if let Some(ret_ty) = &func.return_type
                        && let Type::Generic(g) = ret_ty
                        && g.name == "Result"
                    {
                        // Check Ok and Err types for enums
                        for arg in &g.args {
                            if let Type::Named(named) = arg
                                && self.project.wasi_registry.is_enum(&named.name)
                                && !needed_enums.contains(&named.name)
                            {
                                // Skip ErrorCode for non-sockets interfaces
                                // (they alias it from wasi:cli/types)
                                if named.name == "ErrorCode" && interface_info.package != "sockets"
                                {
                                    continue;
                                }
                                needed_enums.push(named.name.clone());
                            }
                        }
                    }
                }

                // Define and export enum types. The export index (not type index) must be used
                // when referencing the enum in function parameters.
                // In Component Model instance types, both types AND exports increment the item index.
                //
                // Use interface-aware enum lookup to distinguish same-named enums from different
                // interfaces (e.g., wasi:cli/types#ErrorCode vs wasi:sockets/types#ErrorCode)
                let mut enum_type_indices: HashMap<String, u32> = HashMap::new();
                let mut enum_export_indices: HashMap<String, u32> = HashMap::new();

                // Use the full interface path (with version) for enum lookup
                let interface_path = &interface_info.path;

                for enum_name in &needed_enums {
                    if let Some(variants) = project
                        .wasi_registry
                        .get_enum_variants_by_interface(interface_path, enum_name)
                    {
                        // Define the enum type
                        instance_type
                            .ty()
                            .defined_type()
                            .enum_type(variants.iter().map(String::as_str));
                        let type_idx = local_type_idx;
                        local_type_idx += 1;
                        enum_type_indices.insert(enum_name.clone(), type_idx);

                        // Export the enum type immediately (increments the combined index)
                        // Function parameters must use this EXPORT index, not the type index
                        if let Some(cm_name) = project
                            .wasi_registry
                            .get_enum_cm_name_by_interface(interface_path, enum_name)
                        {
                            instance_type.export(
                                cm_name,
                                wasm_encoder::ComponentTypeRef::Type(TypeBounds::Eq(type_idx)),
                            );
                            enum_export_indices.insert(enum_name.clone(), local_type_idx);
                            local_type_idx += 1;
                        }
                    }
                }

                // Track function exports to be added after all type definitions
                let mut deferred_func_exports: Vec<(String, u32)> = Vec::new();

                // Build types for each function (exports will be added at the end)
                for func in &supported_functions {
                    // Determine what types this function needs
                    let needs_stream_u8 = func
                        .params
                        .iter()
                        .any(|(_, ty)| matches!(ty, Type::Generic(g) if g.name == "Stream"));
                    let needs_error_code = func
                        .return_type
                        .as_ref()
                        .is_some_and(|ty| matches!(ty, Type::Generic(g) if g.name == "Result"));
                    // Check if params contain Result (e.g., exit(status: Result<(), ()>))
                    let needs_result_param = func
                        .params
                        .iter()
                        .any(|(_, ty)| matches!(ty, Type::Generic(g) if g.name == "Result"));

                    // Stream<u8> type
                    let stream_type_idx = if needs_stream_u8 {
                        instance_type
                            .ty()
                            .defined_type()
                            .stream(Some(ComponentValType::Primitive(PrimitiveValType::U8)));
                        let idx = local_type_idx;
                        local_type_idx += 1;
                        Some(idx)
                    } else {
                        None
                    };

                    // Error-code type index (if needed for result type)
                    // - Sockets package has its own error-code, use locally-defined one
                    // - CLI package uses error-code from wasi:cli/types, alias the outer one
                    let error_code_idx = if needs_error_code {
                        // Check if this is a sockets interface that has its own ErrorCode
                        let uses_local_error_code = interface_info.package == "sockets"
                            && enum_export_indices.contains_key("ErrorCode");

                        if uses_local_error_code {
                            // Use the locally-defined ErrorCode enum (sockets)
                            Some(enum_export_indices["ErrorCode"])
                        } else {
                            // Alias the outer error-code from wasi:cli/types
                            let outer_error_code = ctx.type_idx("error-code");
                            instance_type.alias(Alias::Outer {
                                kind: ComponentOuterAliasKind::Type,
                                count: 1,
                                index: outer_error_code,
                            });
                            let idx = local_type_idx;
                            local_type_idx += 1;
                            Some(idx)
                        }
                    } else {
                        None
                    };

                    // Result type for return type (with error-code and optional ok type)
                    // For static methods returning Result<Resource, ErrorCode>, use own<resource>
                    let result_type_idx = if let Some(err_idx) = error_code_idx {
                        // Check if the Ok type is a resource (for static methods)
                        let ok_type = if let Some(Type::Generic(g)) = &func.return_type
                            && g.name == "Result"
                            && !g.args.is_empty()
                        {
                            if let Type::Named(named) = &g.args[0]
                                && let Some(&own_idx) = own_resource_type_indices.get(&named.name)
                            {
                                // Ok type is own<resource>
                                Some(ComponentValType::Type(own_idx))
                            } else if let Type::Named(named) = &g.args[0]
                                && named.name == "()"
                            {
                                // Ok type is unit - no payload
                                None
                            } else {
                                // Ok type is a primitive or other type
                                None
                            }
                        } else {
                            None
                        };
                        instance_type
                            .ty()
                            .defined_type()
                            .result(ok_type, Some(ComponentValType::Type(err_idx)));
                        let idx = local_type_idx;
                        local_type_idx += 1;
                        Some(idx)
                    } else {
                        None
                    };

                    // Result type for params (Result<(), ()> - no payloads)
                    let result_param_type_idx = if needs_result_param {
                        // Define a simple result with no ok/error payloads
                        instance_type.ty().defined_type().result(None, None);
                        let idx = local_type_idx;
                        local_type_idx += 1;
                        Some(idx)
                    } else {
                        None
                    };

                    // Array<T> type (CM list type) - define element type first if needed
                    let array_type_idx = if let Some(Type::Generic(g)) = &func.return_type {
                        if g.name == "Array" && !g.args.is_empty() {
                            let element_type = &g.args[0];
                            // Check if element is a tuple type that needs definition
                            let element_val_type = match element_type {
                                // Handle Tuple<T, U> syntax (Type::Generic)
                                Type::Generic(elem_g)
                                    if elem_g.name == "Tuple" && !elem_g.args.is_empty() =>
                                {
                                    // Define tuple type first
                                    let tuple_types: Vec<ComponentValType> =
                                        elem_g.args.iter().map(wado_type_to_cm_primitive).collect();
                                    instance_type.ty().defined_type().tuple(tuple_types);
                                    let tuple_idx = local_type_idx;
                                    local_type_idx += 1;
                                    ComponentValType::Type(tuple_idx)
                                }
                                // Handle [T, U] syntax (Type::Tuple)
                                Type::Tuple(elems) if !elems.is_empty() => {
                                    let tuple_types: Vec<ComponentValType> =
                                        elems.iter().map(wado_type_to_cm_primitive).collect();
                                    instance_type.ty().defined_type().tuple(tuple_types);
                                    let tuple_idx = local_type_idx;
                                    local_type_idx += 1;
                                    ComponentValType::Type(tuple_idx)
                                }
                                _ => wado_type_to_cm_primitive(element_type),
                            };
                            // Define list type
                            instance_type.ty().defined_type().list(element_val_type);
                            let idx = local_type_idx;
                            local_type_idx += 1;
                            Some(idx)
                        } else {
                            None
                        }
                    } else {
                        None
                    };

                    // Option<T> type (CM option type)
                    let option_type_idx = if let Some(Type::Generic(g)) = &func.return_type {
                        if g.name == "Option" && !g.args.is_empty() {
                            let element_type = &g.args[0];
                            let element_val_type = wado_type_to_cm_primitive(element_type);
                            instance_type.ty().defined_type().option(element_val_type);
                            let idx = local_type_idx;
                            local_type_idx += 1;
                            Some(idx)
                        } else {
                            None
                        }
                    } else {
                        None
                    };

                    // Tuple type (CM tuple type) - for [T, U, ...] syntax
                    let tuple_type_idx = if let Some(Type::Tuple(elems)) = &func.return_type {
                        if elems.is_empty() {
                            None
                        } else {
                            let tuple_types: Vec<ComponentValType> =
                                elems.iter().map(wado_type_to_cm_primitive).collect();
                            instance_type.ty().defined_type().tuple(tuple_types);
                            let idx = local_type_idx;
                            local_type_idx += 1;
                            Some(idx)
                        }
                    } else if let Some(Type::Generic(g)) = &func.return_type {
                        // Also handle Tuple<T, U> syntax
                        if g.name == "Tuple" && !g.args.is_empty() {
                            let tuple_types: Vec<ComponentValType> =
                                g.args.iter().map(wado_type_to_cm_primitive).collect();
                            instance_type.ty().defined_type().tuple(tuple_types);
                            let idx = local_type_idx;
                            local_type_idx += 1;
                            Some(idx)
                        } else {
                            None
                        }
                    } else {
                        None
                    };

                    // Build function type
                    // Build params - convert names to kebab-case for CM
                    let kebab_params: Vec<(String, ComponentValType)> = func
                        .params
                        .iter()
                        .map(|(name, ty)| {
                            // Use export indices for enums (required for instance types used as imports)
                            let val_type = self.wado_type_to_cm_val_type(
                                ty,
                                stream_type_idx,
                                error_code_idx,
                                result_param_type_idx,
                                &enum_export_indices,
                            );
                            (to_kebab_case(name), val_type)
                        })
                        .collect();
                    // Convert to references for the encoder
                    let params: Vec<(&str, ComponentValType)> = kebab_params
                        .iter()
                        .map(|(name, val_type)| (name.as_str(), *val_type))
                        .collect();

                    // Build result - resolve type aliases first (e.g., Mark -> u64)
                    let result_type = func.return_type.as_ref().map(|ty| {
                        let resolved_ty = self.project.wasi_registry.resolve_type(ty);
                        self.wado_type_to_cm_result_type(
                            &resolved_ty,
                            result_type_idx,
                            array_type_idx,
                            option_type_idx,
                            tuple_type_idx,
                        )
                    });

                    // Create function type with params, result, and async flag
                    let mut func_encoder = instance_type.ty().function();
                    if func.is_async {
                        func_encoder.async_(true).params(params).result(result_type);
                    } else {
                        func_encoder.params(params).result(result_type);
                    }

                    let func_type_idx = local_type_idx;
                    local_type_idx += 1;

                    // Defer the function export to after all type definitions
                    deferred_func_exports.push((func.wasi_func_name.clone(), func_type_idx));
                }

                // Export functions (enum exports were added inline after each enum definition)
                for (func_name, func_type_idx) in &deferred_func_exports {
                    instance_type.export(
                        func_name,
                        wasm_encoder::ComponentTypeRef::Func(*func_type_idx),
                    );
                }

                enc.instance(&instance_type);
            }

            // Import the interface instance
            ctx.register_instance(&interface_info.interface);
            builder.import(
                &interface_info.path,
                wasm_encoder::ComponentTypeRef::Instance(instance_type_idx),
            );

            // Alias each function from the instance
            for func in &supported_functions {
                let local_name = project
                    .wasi_registry
                    .get_local_name(&interface_info.path, &func.wasi_func_name)
                    .cloned()
                    .unwrap_or_else(|| {
                        format!("{}-{}", interface_info.interface, func.wasi_func_name)
                    });

                ctx.register_comp_func(&local_name);
                builder.alias_export(
                    ctx.instance_idx(&interface_info.interface),
                    &func.wasi_func_name,
                    ComponentExportKind::Func,
                );
            }
        }

        // Import interfaces with resource types (terminal-stdin, terminal-stdout, terminal-stderr)
        // using registry data instead of hardcoded effect names
        self.import_interfaces_with_resources(builder, ctx, project);

        // For Service world, import wasi:http/types to get Request resource type
        // This is needed for the handle function's parameter type
        if self.project.has_http_handler_export {
            self.import_http_types_for_service(builder, ctx);
        }
    }

    /// Import wasi:http/types for Service world
    ///
    /// This imports the HTTP types interface which defines the Request resource type
    /// needed for the handler export function.
    fn import_http_types_for_service(
        &self,
        builder: &mut ComponentBuilder,
        ctx: &mut ComponentModelContext,
    ) {
        // HTTP types interface exports request, response, fields (resources), error-code (variant)
        // We also need [constructor]fields and [static]response.new for response creation
        //
        // Type indices within instance type (created by ty() calls, NOT by SubResource exports):
        // SubResource exports don't create type indices - they're placeholders
        // 0: error-code (variant)
        // 1: stream<u8>
        // 2: option<stream<u8>>
        // 3: result<_, error-code> (for transmission future)
        // 4: future<result<_, error-code>>
        // 5: [constructor]fields function type
        // 6: [static]response.new function type (simplified - just takes i32s and returns i32s)
        let http_types_instance_type = ctx.register_type("http-types-instance-type");
        {
            let (_, enc) = builder.ty(Some("http-types-instance-type"));
            let mut instance_type = InstanceType::new();

            // Type 0: request (sub-resource export)
            instance_type.export(
                "request",
                wasm_encoder::ComponentTypeRef::Type(TypeBounds::SubResource),
            );
            // Type 1: response (sub-resource export)
            instance_type.export(
                "response",
                wasm_encoder::ComponentTypeRef::Type(TypeBounds::SubResource),
            );

            // Type 2: fields (sub-resource export)
            instance_type.export(
                "fields",
                wasm_encoder::ComponentTypeRef::Type(TypeBounds::SubResource),
            );

            // === Payload types for error-code variant ===
            // Records and variants must be "named" (exported) to be used in function signatures.
            // The full error-code type matches wasmtime's wasi:http/types interface.

            // Type 3: option<string> (used in multiple record payloads)
            instance_type
                .ty()
                .defined_type()
                .option(ComponentValType::Primitive(PrimitiveValType::String));

            // Type 4: option<u16> (for DNS-error-payload.info-code)
            instance_type
                .ty()
                .defined_type()
                .option(ComponentValType::Primitive(PrimitiveValType::U16));

            // Type 5: DNS-error-payload record { rcode: option<string>, info-code: option<u16> }
            instance_type.ty().defined_type().record([
                ("rcode", ComponentValType::Type(3)),     // option<string>
                ("info-code", ComponentValType::Type(4)), // option<u16>
            ]);
            // Type 6: Export DNS-error-payload to make it "named"
            instance_type.export(
                "DNS-error-payload",
                wasm_encoder::ComponentTypeRef::Type(TypeBounds::Eq(5)),
            );

            // Type 7: option<u8> (for TLS-alert-received-payload.alert-id)
            instance_type
                .ty()
                .defined_type()
                .option(ComponentValType::Primitive(PrimitiveValType::U8));

            // Type 8: TLS-alert-received-payload record
            instance_type.ty().defined_type().record([
                ("alert-id", ComponentValType::Type(7)),      // option<u8>
                ("alert-message", ComponentValType::Type(3)), // option<string>
            ]);
            // Type 9: Export TLS-alert-received-payload to make it "named"
            instance_type.export(
                "TLS-alert-received-payload",
                wasm_encoder::ComponentTypeRef::Type(TypeBounds::Eq(8)),
            );

            // Type 10: option<u32> (for field-size-payload.field-size and some variant payloads)
            instance_type
                .ty()
                .defined_type()
                .option(ComponentValType::Primitive(PrimitiveValType::U32));

            // Type 11: field-size-payload record
            instance_type.ty().defined_type().record([
                ("field-name", ComponentValType::Type(3)), // option<string>
                ("field-size", ComponentValType::Type(10)), // option<u32>
            ]);
            // Type 12: Export field-size-payload to make it "named"
            instance_type.export(
                "field-size-payload",
                wasm_encoder::ComponentTypeRef::Type(TypeBounds::Eq(11)),
            );

            // Type 13: option<u64> (for HTTP-request-body-size, HTTP-response-body-size)
            instance_type
                .ty()
                .defined_type()
                .option(ComponentValType::Primitive(PrimitiveValType::U64));

            // Type 14: option<field-size-payload>
            // Must use the exported alias (type 12) for the named field-size-payload
            instance_type
                .ty()
                .defined_type()
                .option(ComponentValType::Type(12));

            // Type 15: error-code variant with proper payloads
            // Use named (exported) types for record payloads:
            // - type 6 for DNS-error-payload
            // - type 9 for TLS-alert-received-payload
            // - type 12 for field-size-payload
            instance_type.ty().defined_type().variant([
                ("DNS-timeout", None, None),
                ("DNS-error", Some(ComponentValType::Type(6)), None), // DNS-error-payload
                ("destination-not-found", None, None),
                ("destination-unavailable", None, None),
                ("destination-IP-prohibited", None, None),
                ("destination-IP-unroutable", None, None),
                ("connection-refused", None, None),
                ("connection-terminated", None, None),
                ("connection-timeout", None, None),
                ("connection-read-timeout", None, None),
                ("connection-write-timeout", None, None),
                ("connection-limit-reached", None, None),
                ("TLS-protocol-error", None, None),
                ("TLS-certificate-error", None, None),
                ("TLS-alert-received", Some(ComponentValType::Type(9)), None), // TLS-alert-received-payload
                ("HTTP-request-denied", None, None),
                ("HTTP-request-length-required", None, None),
                (
                    "HTTP-request-body-size",
                    Some(ComponentValType::Type(13)),
                    None,
                ), // option<u64>
                ("HTTP-request-method-invalid", None, None),
                ("HTTP-request-URI-invalid", None, None),
                ("HTTP-request-URI-too-long", None, None),
                (
                    "HTTP-request-header-section-size",
                    Some(ComponentValType::Type(10)),
                    None,
                ), // option<u32>
                (
                    "HTTP-request-header-size",
                    Some(ComponentValType::Type(14)),
                    None,
                ), // option<field-size-payload>
                (
                    "HTTP-request-trailer-section-size",
                    Some(ComponentValType::Type(10)),
                    None,
                ), // option<u32>
                (
                    "HTTP-request-trailer-size",
                    Some(ComponentValType::Type(12)),
                    None,
                ), // field-size-payload
                ("HTTP-response-incomplete", None, None),
                (
                    "HTTP-response-header-section-size",
                    Some(ComponentValType::Type(10)),
                    None,
                ), // option<u32>
                (
                    "HTTP-response-header-size",
                    Some(ComponentValType::Type(12)),
                    None,
                ), // field-size-payload
                (
                    "HTTP-response-body-size",
                    Some(ComponentValType::Type(13)),
                    None,
                ), // option<u64>
                (
                    "HTTP-response-trailer-section-size",
                    Some(ComponentValType::Type(10)),
                    None,
                ), // option<u32>
                (
                    "HTTP-response-trailer-size",
                    Some(ComponentValType::Type(12)),
                    None,
                ), // field-size-payload
                (
                    "HTTP-response-transfer-coding",
                    Some(ComponentValType::Type(3)),
                    None,
                ), // option<string>
                (
                    "HTTP-response-content-coding",
                    Some(ComponentValType::Type(3)),
                    None,
                ), // option<string>
                ("HTTP-response-timeout", None, None),
                ("HTTP-upgrade-failed", None, None),
                ("HTTP-protocol-error", None, None),
                ("loop-detected", None, None),
                ("configuration-error", None, None),
                ("internal-error", Some(ComponentValType::Type(3)), None), // option<string>
            ]);
            // Type 16: Export error-code to make it "named"
            instance_type.export(
                "error-code",
                wasm_encoder::ComponentTypeRef::Type(TypeBounds::Eq(15)),
            );

            // Type indices (full error-code):
            // 0: request (sub-resource export - named)
            // 1: response (sub-resource export - named)
            // 2: fields (sub-resource export - named)
            // 3: option<string>
            // 4: option<u16>
            // 5: DNS-error-payload record (internal)
            // 6: DNS-error-payload (Eq export - named)
            // 7: option<u8>
            // 8: TLS-alert-received-payload record (internal)
            // 9: TLS-alert-received-payload (Eq export - named)
            // 10: option<u32>
            // 11: field-size-payload record (internal)
            // 12: field-size-payload (Eq export - named)
            // 13: option<u64>
            // 14: option<field-size-payload>
            // 15: error-code variant (internal)
            // 16: error-code (Eq export - named)
            // 17: stream<u8>
            // 18: option<stream<u8>>
            // 19: result<_, error-code>
            // 20: future<result<_, error-code>>

            // Type 17: stream<u8>
            instance_type
                .ty()
                .defined_type()
                .stream(Some(ComponentValType::Primitive(PrimitiveValType::U8)));

            // Type 18: option<stream<u8>>
            instance_type
                .ty()
                .defined_type()
                .option(ComponentValType::Type(17));

            // Type 19: result<_, error-code> (for transmission future)
            // Use type 16 (named/exported error-code alias)
            instance_type
                .ty()
                .defined_type()
                .result(None, Some(ComponentValType::Type(16)));

            // Type 20: future<result<_, error-code>>
            instance_type
                .ty()
                .defined_type()
                .future(Some(ComponentValType::Type(19)));

            // Types for [constructor]fields and [static]response.new
            // Type 21: own<fields> (for constructor return and parameters)
            instance_type.ty().defined_type().own(2); // fields is at export index 2

            // Type 22: own<response> (for response.new return)
            instance_type.ty().defined_type().own(1); // response is at export index 1

            // Type 23: option<own<fields>> (for trailers in result)
            instance_type
                .ty()
                .defined_type()
                .option(ComponentValType::Type(21));

            // Type 24: result<option<own<fields>>, error-code> (for trailers future payload)
            instance_type.ty().defined_type().result(
                Some(ComponentValType::Type(23)),
                Some(ComponentValType::Type(16)), // error-code export
            );

            // Type 25: future<result<option<own<fields>>, error-code>> (trailers parameter)
            instance_type
                .ty()
                .defined_type()
                .future(Some(ComponentValType::Type(24)));

            // Type 26: tuple<own<response>, future<result<_, error-code>>> (response.new return)
            instance_type.ty().defined_type().tuple([
                ComponentValType::Type(22), // own<response>
                ComponentValType::Type(20), // future<result<_, error-code>>
            ]);

            // Type 27: [constructor]fields function type
            // Signature: () -> own<fields>
            // Note: constructors are NOT async
            let params: [(&str, ComponentValType); 0] = [];
            instance_type
                .ty()
                .function()
                .params(params)
                .result(Some(ComponentValType::Type(21)));

            // Type 28: [static]response.new function type
            // Signature: (headers: own<fields>, contents: option<stream<u8>>,
            //             trailers: future<result<option<own<fields>>, error-code>>)
            //         -> tuple<own<response>, future<result<_, error-code>>>
            // Note: NOT async - static functions with futures are still sync in CM
            instance_type
                .ty()
                .function()
                .params([
                    ("headers", ComponentValType::Type(21)),  // own<fields>
                    ("contents", ComponentValType::Type(18)), // option<stream<u8>>
                    ("trailers", ComponentValType::Type(25)), // future<...>
                ])
                .result(Some(ComponentValType::Type(26))); // tuple<response, future>

            // Export [constructor]fields function (type 27)
            instance_type.export(
                "[constructor]fields",
                wasm_encoder::ComponentTypeRef::Func(27),
            );

            // Export [static]response.new function (type 28)
            instance_type.export(
                "[static]response.new",
                wasm_encoder::ComponentTypeRef::Func(28),
            );

            enc.instance(&instance_type);
        }

        // Import the wasi:http/types instance
        ctx.register_instance("http-types");
        let http_version = "0.3.0-rc-2026-01-06";
        let http_types_import_path = format!("wasi:http/types@{http_version}");
        builder.import(
            &http_types_import_path,
            wasm_encoder::ComponentTypeRef::Instance(http_types_instance_type),
        );

        // Alias the request resource type
        ctx.register_type("http-request-resource");
        builder.alias_export(
            ctx.instance_idx("http-types"),
            "request",
            ComponentExportKind::Type,
        );

        // Alias the response resource type
        ctx.register_type("http-response-resource");
        builder.alias_export(
            ctx.instance_idx("http-types"),
            "response",
            ComponentExportKind::Type,
        );

        // Alias the fields resource type
        ctx.register_type("http-fields-resource");
        builder.alias_export(
            ctx.instance_idx("http-types"),
            "fields",
            ComponentExportKind::Type,
        );

        // Alias the error-code type
        ctx.register_type("http-error-code");
        builder.alias_export(
            ctx.instance_idx("http-types"),
            "error-code",
            ComponentExportKind::Type,
        );

        // Alias [constructor]fields function for creating empty headers
        ctx.register_comp_func("http-fields-constructor");
        builder.alias_export(
            ctx.instance_idx("http-types"),
            "[constructor]fields",
            ComponentExportKind::Func,
        );

        // Alias [static]response.new function for creating responses
        ctx.register_comp_func("http-response-new");
        builder.alias_export(
            ctx.instance_idx("http-types"),
            "[static]response.new",
            ComponentExportKind::Func,
        );

        // NOTE: The lowering of these functions is done in lower_http_response_functions()
        // which is called after generate_http_response_types().

        // Define own<request> type for use in function params
        let request_resource_idx = ctx.type_idx("http-request-resource");
        ctx.register_type("http-request");
        {
            let (_, enc) = builder.ty(Some("http-request"));
            enc.defined_type().own(request_resource_idx);
        }

        // Define own<response> type for use in result
        let response_resource_idx = ctx.type_idx("http-response-resource");
        ctx.register_type("http-response");
        {
            let (_, enc) = builder.ty(Some("http-response"));
            enc.defined_type().own(response_resource_idx);
        }

        // Define result<own<response>, error-code> type for the handler return type
        let response_type_idx = ctx.type_idx("http-response");
        let error_code_type_idx = ctx.type_idx("http-error-code");
        ctx.register_type("http-handler-result");
        {
            let (_, enc) = builder.ty(Some("http-handler-result"));
            enc.defined_type().result(
                Some(ComponentValType::Type(response_type_idx)),
                Some(ComponentValType::Type(error_code_type_idx)),
            );
        }

        // Note: Additional types for HTTP response creation (fields, trailers future, etc.)
        // will be defined later in generate_http_response_types() when stream-u8 is available.
    }

    /// Import an interface that has a resource type, using registry data.
    ///
    /// This handles interfaces like terminal-stdin, terminal-stdout, terminal-stderr
    /// that export a resource type and have functions returning `Option<Own<Resource>>`.
    fn import_interface_with_resource(
        &self,
        builder: &mut ComponentBuilder,
        ctx: &mut ComponentModelContext,
        interface_info: &WasiInterfaceInfo,
        project: &Project,
    ) {
        // Get resource type info
        let Some((_, resource_cm_name)) = &interface_info.resource_type else {
            return;
        };

        // Get the first function (interfaces with resources typically have one function)
        let Some(func) = interface_info.functions.first() else {
            return;
        };

        let local_name = func.local_alias_name();

        // Check if this effect is used and function isn't already imported
        if !project.has_effect(&func.effect_name) || ctx.has_comp_func(&local_name) {
            return;
        }

        // Build the instance type name from interface name
        let instance_type_name = format!("{}-instance-type", interface_info.interface);
        let instance_type_idx = ctx.register_type(&instance_type_name);
        {
            let (_, enc) = builder.ty(Some(&instance_type_name));
            let mut instance_type = InstanceType::new();

            // Type 0: resource (SubResource for imported resource)
            instance_type.export(
                resource_cm_name,
                wasm_encoder::ComponentTypeRef::Type(TypeBounds::SubResource),
            );

            // Type 1: own<resource>
            instance_type.ty().defined_type().own(0);

            // Type 2: option<own<resource>>
            instance_type
                .ty()
                .defined_type()
                .option(ComponentValType::Type(1));

            // Type 3: func() -> option<own<resource>>
            instance_type
                .ty()
                .function()
                .params::<[(&str, ComponentValType); 0], _>([])
                .result(Some(ComponentValType::Type(2)));

            instance_type.export(
                &func.wasi_func_name,
                wasm_encoder::ComponentTypeRef::Func(3),
            );

            enc.instance(&instance_type);
        }

        ctx.register_instance(&interface_info.interface);
        builder.import(
            &interface_info.path,
            wasm_encoder::ComponentTypeRef::Instance(instance_type_idx),
        );

        // Export the function
        ctx.register_comp_func(&local_name);
        builder.alias_export(
            ctx.instance_idx(&interface_info.interface),
            &func.wasi_func_name,
            ComponentExportKind::Func,
        );
    }

    /// Import all interfaces with resource types from the registry.
    ///
    /// This replaces the hardcoded terminal-stdin/stdout/stderr import functions
    /// with a data-driven approach that iterates over the registry.
    fn import_interfaces_with_resources(
        &self,
        builder: &mut ComponentBuilder,
        ctx: &mut ComponentModelContext,
        project: &Project,
    ) {
        for interface_info in self.project.wasi_registry.interfaces() {
            // Only handle interfaces that have a resource type
            if interface_info.resource_type.is_some() {
                self.import_interface_with_resource(builder, ctx, &interface_info, project);
            }
        }
    }

    /// Generate `canon lower` calls for all registered WASI functions.
    ///
    /// This method iterates over all functions in the WASI registry and generates
    /// the appropriate `canon lower` calls based on their `CmCallConvention`.
    /// The canonical options are derived from the convention:
    /// - `is_async` → `CanonicalOption::Async`
    /// - `needs_memory` → `CanonicalOption::Memory`
    /// - `needs_realloc` → `CanonicalOption::Realloc`
    fn lower_wasi_functions(
        &self,
        builder: &mut ComponentBuilder,
        ctx: &mut ComponentModelContext,
    ) {
        // Iterate over all interfaces and their functions
        for interface_info in self.project.wasi_registry.interfaces() {
            for func in &interface_info.functions {
                let local_name = func.local_alias_name();

                // Only lower if the component function was imported
                if !ctx.has_comp_func(&local_name) {
                    continue;
                }

                // Skip async functions with void return - they have a different ABI
                // that isn't fully supported yet (e.g., wait_until, wait_for)
                // Note: async functions with Result<T, E> return (like write_via_stream) are OK
                if func.is_async && func.return_type.is_none() {
                    continue;
                }

                // Register the core function with the same name
                ctx.register_core_func(&local_name);

                // Build canonical options based on call convention
                let conv = &func.call_convention;
                let mut options: Vec<CanonicalOption> = Vec::new();

                if conv.is_async {
                    options.push(CanonicalOption::Async);
                }
                if conv.needs_memory {
                    options.push(CanonicalOption::Memory(ctx.memory_idx()));
                }
                if conv.needs_realloc {
                    options.push(CanonicalOption::Realloc(ctx.core_func_idx("realloc")));
                }

                // Lower the component function to a core function
                builder.lower_func(Some(&local_name), ctx.comp_func_idx(&local_name), options);
            }
        }
    }

    /// Convert a Wado type to a Component Model value type
    ///
    /// Panics if the type is not supported - callers must validate with
    /// `is_param_type_supported` first. Type aliases should already be resolved.
    fn wado_type_to_cm_val_type(
        &self,
        ty: &Type,
        stream_type_idx: Option<u32>,
        _error_code_idx: Option<u32>,
        result_param_type_idx: Option<u32>,
        enum_type_indices: &HashMap<String, u32>,
    ) -> ComponentValType {
        match ty {
            Type::Named(named) => {
                // Check if it's a known enum type first
                if let Some(&enum_idx) = enum_type_indices.get(&named.name) {
                    return ComponentValType::Type(enum_idx);
                }
                // Otherwise, check primitives
                match named.name.as_str() {
                    "i32" => ComponentValType::Primitive(PrimitiveValType::S32),
                    "i64" => ComponentValType::Primitive(PrimitiveValType::S64),
                    "u8" => ComponentValType::Primitive(PrimitiveValType::U8),
                    "u16" => ComponentValType::Primitive(PrimitiveValType::U16),
                    "u32" => ComponentValType::Primitive(PrimitiveValType::U32),
                    "u64" => ComponentValType::Primitive(PrimitiveValType::U64),
                    "f32" => ComponentValType::Primitive(PrimitiveValType::F32),
                    "f64" => ComponentValType::Primitive(PrimitiveValType::F64),
                    "bool" => ComponentValType::Primitive(PrimitiveValType::Bool),
                    "char" => ComponentValType::Primitive(PrimitiveValType::Char),
                    "String" => ComponentValType::Primitive(PrimitiveValType::String),
                    _ => panic!("unsupported Wado param type for CM: {}", named.name),
                }
            }
            Type::Generic(generic) => match generic.name.as_str() {
                "Stream" => {
                    // Use the pre-defined stream type index
                    ComponentValType::Type(stream_type_idx.expect("stream type not defined"))
                }
                "Result" => {
                    // Use the pre-defined result type index for params (Result<(), ()>)
                    ComponentValType::Type(
                        result_param_type_idx.expect("result param type not defined"),
                    )
                }
                _ => panic!("unsupported generic param type for CM: {}", generic.name),
            },
            _ => panic!("unsupported Wado param type for CM: {ty:?}"),
        }
    }

    /// Convert a Wado return type to a Component Model result type
    ///
    /// Panics if the type is not supported - callers must validate with
    /// `is_return_type_supported` first. Type aliases should already be resolved.
    fn wado_type_to_cm_result_type(
        &self,
        ty: &Type,
        result_type_idx: Option<u32>,
        array_type_idx: Option<u32>,
        option_type_idx: Option<u32>,
        tuple_type_idx: Option<u32>,
    ) -> ComponentValType {
        match ty {
            Type::Named(named) => match named.name.as_str() {
                "i32" => ComponentValType::Primitive(PrimitiveValType::S32),
                "i64" => ComponentValType::Primitive(PrimitiveValType::S64),
                "u8" => ComponentValType::Primitive(PrimitiveValType::U8),
                "u16" => ComponentValType::Primitive(PrimitiveValType::U16),
                "u32" => ComponentValType::Primitive(PrimitiveValType::U32),
                "u64" => ComponentValType::Primitive(PrimitiveValType::U64),
                "f32" => ComponentValType::Primitive(PrimitiveValType::F32),
                "f64" => ComponentValType::Primitive(PrimitiveValType::F64),
                "bool" => ComponentValType::Primitive(PrimitiveValType::Bool),
                "char" => ComponentValType::Primitive(PrimitiveValType::Char),
                "String" => ComponentValType::Primitive(PrimitiveValType::String),
                _ => panic!("unsupported Wado return type for CM: {}", named.name),
            },
            Type::Generic(generic) => match generic.name.as_str() {
                "Result" => {
                    // Use the pre-defined result type index
                    ComponentValType::Type(result_type_idx.expect("result type not defined"))
                }
                "Array" => {
                    // Use the pre-defined array/list type index
                    ComponentValType::Type(array_type_idx.expect("array type not defined"))
                }
                "Option" => {
                    // Use the pre-defined option type index
                    ComponentValType::Type(option_type_idx.expect("option type not defined"))
                }
                "Tuple" => {
                    // Use the pre-defined tuple type index (Tuple<...> syntax)
                    ComponentValType::Type(tuple_type_idx.expect("tuple type not defined"))
                }
                _ => panic!("unsupported generic return type for CM: {}", generic.name),
            },
            // Handle [...] tuple syntax
            Type::Tuple(_) => {
                ComponentValType::Type(tuple_type_idx.expect("tuple type not defined"))
            }
            _ => panic!("unsupported Wado return type for CM: {ty:?}"),
        }
    }

    /// Get the offset of a string in the string data section
    fn get_string_offset(&self, s: &str) -> u32 {
        let mut offset = 0u32;
        for lit in &self.string_literals {
            if lit == s {
                return offset;
            }
            offset += lit.len() as u32;
        }
        panic!("String not found in literals: {s}");
    }

    /// Check if a struct contains type parameters in any of its field types
    /// Returns true if any field has an unresolved type parameter
    fn struct_contains_type_params(
        &self,
        tir_struct: &crate::tir::TirStruct,
        type_table: &TypeTable,
    ) -> bool {
        for field in &tir_struct.fields {
            if type_table.contains_type_param(field.type_id) {
                return true;
            }
        }
        false
    }

    /// Register a struct type from TIR with a `StructName` key
    fn register_struct_type(
        &mut self,
        struct_name: StructName,
        tir_struct: &crate::tir::TirStruct,
        type_table: &TypeTable,
        builder: &mut CoreModuleBuilder,
    ) -> u32 {
        // Pre-register any GenericInstance field types that aren't yet registered.
        // This handles cases where a struct field depends on a GenericInstance that
        // was excluded from the normal struct registration flow (e.g., Array types).
        for field in &tir_struct.fields {
            self.ensure_field_generic_types_registered(field.type_id, type_table, builder);
        }

        let mut fields = Vec::new();

        for field in &tir_struct.fields {
            let wasm_type = self.type_id_to_valtype(type_table, field.type_id);
            let storage_type = match wasm_type {
                ValType::I32 => StorageType::Val(ValType::I32),
                ValType::I64 => StorageType::Val(ValType::I64),
                ValType::F32 => StorageType::Val(ValType::F32),
                ValType::F64 => StorageType::Val(ValType::F64),
                ValType::Ref(rt) => StorageType::Val(ValType::Ref(rt)),
                _ => StorageType::Val(ValType::I32), // Default fallback
            };
            fields.push(FieldType {
                element_type: storage_type,
                mutable: true, // All fields are mutable by default
            });
        }

        // Use the struct name's string representation for Wasm type naming
        let type_idx = builder.define_gc_struct_type(&struct_name.name, &fields);

        let field_count = tir_struct.fields.len();
        let is_monomorphized = tir_struct.monomorph_info.is_some();
        let base_name = tir_struct
            .monomorph_info
            .as_ref()
            .map(|info| info.generic_name.clone());
        self.struct_types.insert(
            struct_name,
            StructTypeInfo {
                type_idx,
                field_count,
                is_monomorphized,
                base_name,
            },
        );

        type_idx
    }

    /// Register a self-referential struct using Wasm GC rec groups.
    ///
    /// For structs like `BTreeNode<K,V> { children: Array<&mut BTreeNode<K,V>> }`,
    /// we need to use rec groups so that the types can forward-reference each other.
    ///
    /// This function registers:
    /// 1. The raw GC array type for self-referential element
    /// 2. The Array<T> struct type for the self-referential field
    /// 3. The main struct type
    ///
    /// All in a single rec group so they can mutually reference each other.
    fn register_self_referential_struct(
        &mut self,
        struct_name: StructName,
        tir_struct: &crate::tir::TirStruct,
        self_ref_field_types: &[TypeId],
        type_table: &TypeTable,
        builder: &mut CoreModuleBuilder,
    ) -> u32 {
        // Collect all types needed in the rec group:
        // For each self-referential field type (e.g., Array<&mut BTreeNode>):
        // 1. The raw GC array type (array of ref $struct)
        // 2. The Array struct type (repr, used)
        // Plus the main struct type itself

        let base_idx = builder.peek_next_type_idx();
        let mut rec_types: Vec<(String, RecTypeKind)> = Vec::new();
        let mut array_type_mappings: Vec<(TypeId, u32, u32)> = Vec::new(); // (element_type_id, raw_array_idx, array_struct_idx)

        // First, add the raw GC array types and Array struct types for self-referential fields
        for &field_type_id in self_ref_field_types {
            // Extract the element type from Array<T>
            if let ResolvedType::GenericInstance {
                name, type_args, ..
            } = type_table.get(field_type_id)
                && name == "Array"
                && type_args.len() == 1
            {
                let element_type_id = type_args[0];

                // Skip if already registered
                if self
                    .lookup_array_struct_type(element_type_id, type_table)
                    .is_some()
                {
                    continue;
                }

                let raw_array_idx = base_idx + rec_types.len() as u32;
                let array_struct_idx = base_idx + rec_types.len() as u32 + 1;

                // 1. Raw GC array type with forward reference to struct
                // The struct will be the last type in the rec group
                let struct_idx_in_rec = base_idx + rec_types.len() as u32 + 2; // Estimate: after raw array and Array struct
                let raw_array_name = format!("array_{element_type_id}");
                let element_storage = StorageType::Val(ValType::Ref(RefType {
                    nullable: true, // Must be nullable for array.new_default
                    heap_type: HeapType::Concrete(struct_idx_in_rec),
                }));
                rec_types.push((
                    raw_array_name.clone(),
                    RecTypeKind::Array(FieldType {
                        element_type: element_storage,
                        mutable: true,
                    }),
                ));

                // 2. Array struct type (use mangled name for consistency)
                let elem_mangled = type_table.mangle_type_name(element_type_id);
                let array_struct_name = mangle_generic_name("Array", &[elem_mangled]);
                let array_struct_fields = vec![
                    FieldType {
                        element_type: StorageType::Val(ValType::Ref(RefType {
                            nullable: true,
                            heap_type: HeapType::Concrete(raw_array_idx),
                        })),
                        mutable: true,
                    },
                    FieldType {
                        element_type: StorageType::Val(ValType::I32),
                        mutable: true,
                    },
                ];
                rec_types.push((array_struct_name, RecTypeKind::Struct(array_struct_fields)));

                array_type_mappings.push((element_type_id, raw_array_idx, array_struct_idx));
            }
        }

        // Now add the main struct type
        let struct_type_idx = base_idx + rec_types.len() as u32;
        let mut struct_fields = Vec::new();

        for field in &tir_struct.fields {
            // Check if this field is in the self-referential list
            let is_self_ref = self_ref_field_types.contains(&field.type_id);
            let wasm_type = if is_self_ref {
                // Check if it's an Array type (Array<&mut T> pattern)
                if let ResolvedType::GenericInstance {
                    name, type_args, ..
                } = type_table.get(field.type_id)
                    && name == "Array"
                {
                    // Look up the Array struct type we planned in the rec group
                    if let Some(&(_, _, array_struct_idx)) = array_type_mappings
                        .iter()
                        .find(|(elem_id, _, _)| *elem_id == type_args[0])
                    {
                        ValType::Ref(RefType {
                            nullable: false,
                            heap_type: HeapType::Concrete(array_struct_idx),
                        })
                    } else {
                        // Fallback - shouldn't happen for properly detected Array self-refs
                        self.type_id_to_valtype(type_table, field.type_id)
                    }
                } else {
                    // Non-Array self-reference (e.g., Option<&mut Self>, &mut Self)
                    // Use forward reference to the struct being defined
                    Self::type_id_to_valtype_with_self_ref(
                        type_table,
                        field.type_id,
                        struct_type_idx,
                    )
                }
            } else {
                self.type_id_to_valtype(type_table, field.type_id)
            };

            let storage_type = match wasm_type {
                ValType::I32 => StorageType::Val(ValType::I32),
                ValType::I64 => StorageType::Val(ValType::I64),
                ValType::F32 => StorageType::Val(ValType::F32),
                ValType::F64 => StorageType::Val(ValType::F64),
                ValType::Ref(rt) => StorageType::Val(ValType::Ref(rt)),
                _ => StorageType::Val(ValType::I32),
            };
            struct_fields.push(FieldType {
                element_type: storage_type,
                mutable: true,
            });
        }

        rec_types.push((struct_name.name.clone(), RecTypeKind::Struct(struct_fields)));

        // Define the rec group
        let indices = builder.define_rec_group(&rec_types);

        // Register the array types in our tracking maps
        for (element_type_id, raw_array_idx, array_struct_idx) in array_type_mappings {
            // Register raw array type
            self.array_types.insert(element_type_id, raw_array_idx);
            let canonical_name = self.canonical_element_type_name(element_type_id, type_table);
            self.array_types_by_name
                .insert(canonical_name.clone(), raw_array_idx);

            // Register Array struct type in struct_types (unified with other generic structs)
            let elem_mangled = type_table.mangle_type_name(element_type_id);
            let array_struct_mangled = mangle_generic_name("Array", &[elem_mangled]);
            let array_struct_name =
                StructName::new(ModuleSource::entry_point(), array_struct_mangled);
            self.struct_types.insert(
                array_struct_name,
                StructTypeInfo {
                    type_idx: array_struct_idx,
                    field_count: 2, // repr and used
                    is_monomorphized: true,
                    base_name: Some("Array".to_string()),
                },
            );
        }

        // Register the struct type
        let field_count = tir_struct.fields.len();
        let is_monomorphized = tir_struct.monomorph_info.is_some();
        let base_name = tir_struct
            .monomorph_info
            .as_ref()
            .map(|info| info.generic_name.clone());
        self.struct_types.insert(
            struct_name,
            StructTypeInfo {
                type_idx: struct_type_idx,
                field_count,
                is_monomorphized,
                base_name,
            },
        );

        // Return the last index (the struct type)
        *indices.last().unwrap()
    }

    /// Register a custom variant type as a Wasm GC struct hierarchy.
    ///
    /// Uses subtype-based representation:
    /// - Base type: (tag: i32) - contains only the discriminator
    /// - Case types: subtypes of base with case-specific payload fields
    ///
    /// Example for `variant JsonValue { Null, Number(f64), Str(String) }`:
    /// - Base: `$JsonValue (struct (field i32))`
    /// - `$JsonValue::Null (sub $JsonValue (struct (field i32)))`
    /// - `$JsonValue::Number (sub $JsonValue (struct (field i32) (field f64)))`
    /// - `$JsonValue::Str (sub $JsonValue (struct (field i32) (field (ref $String))))`
    fn register_variant_type(
        &mut self,
        variant: &crate::tir::TirVariantDecl,
        type_table: &TypeTable,
        builder: &mut CoreModuleBuilder,
    ) -> u32 {
        // Skip generic variants (they will be registered when monomorphized)
        if !variant.type_params.is_empty() {
            return u32::MAX;
        }

        // Already registered?
        if self.variant_types.contains_key(&variant.name) {
            return self.variant_types[&variant.name].base_type_idx;
        }

        // Define the base type with just the tag field
        let base_fields = vec![FieldType {
            element_type: StorageType::Val(ValType::I32),
            mutable: false, // Tag is immutable once set
        }];
        let base_type_idx = builder.define_gc_struct_type(&variant.name, &base_fields);

        // Define each case as a subtype
        let mut case_infos = Vec::with_capacity(variant.cases.len());

        for case in &variant.cases {
            // Build fields for this case: tag + optional payload field
            let mut case_fields = vec![FieldType {
                element_type: StorageType::Val(ValType::I32),
                mutable: false,
            }];

            // Each variant case has exactly one payload type.
            // Unit variants (payload is unit type) have no payload field.
            let payload_is_unit = matches!(type_table.get(case.payload), ResolvedType::Unit);
            let payload_type = if payload_is_unit {
                None
            } else {
                let wasm_type = self.type_id_to_valtype(type_table, case.payload);
                case_fields.push(FieldType {
                    element_type: StorageType::Val(wasm_type),
                    mutable: true,
                });
                Some(wasm_type)
            };

            // Define the case subtype
            let case_type_name = format!("{}::{}", variant.name, case.name);
            let case_type_idx =
                builder.define_gc_struct_subtype(&case_type_name, base_type_idx, &case_fields);

            case_infos.push(VariantCaseInfo {
                name: case.name.clone(),
                type_idx: case_type_idx,
                payload_type,
            });
        }

        // Store in registry
        self.variant_types.insert(
            variant.name.clone(),
            VariantTypeInfo {
                base_type_idx,
                cases: case_infos,
            },
        );

        base_type_idx
    }

    /// Register monomorphized generic variants from the type table.
    ///
    /// Scans for `GenericInstance` types that refer to variants (like `Result<i32, String>`)
    /// and registers them as concrete variant types using subtype hierarchy.
    fn register_monomorphized_variants_from_table(
        &mut self,
        tir_module: &TirModule,
        type_table: &TypeTable,
        builder: &mut CoreModuleBuilder,
    ) {
        // Collect all GenericInstance types that are variants
        let mut generic_variants: Vec<(String, Vec<TypeId>)> = Vec::new();

        for type_id in type_table.iter_type_ids() {
            if let ResolvedType::GenericInstance {
                name, type_args, ..
            } = type_table.get(type_id)
            {
                // Check if this is a variant (has a declaration in tir_module.variants)
                let is_variant = tir_module.variants.iter().any(|v| v.name == *name);
                if is_variant {
                    let mangled_name = type_table.mangle_type_name(type_id);
                    // Skip if already registered
                    if !self.variant_types.contains_key(&mangled_name) {
                        generic_variants.push((name.clone(), type_args.clone()));
                    }
                }
            }
        }

        // Register each monomorphized variant
        for (base_name, type_args) in generic_variants {
            // Find the base variant declaration
            let base_variant = tir_module
                .variants
                .iter()
                .find(|v| v.name == base_name)
                .expect("variant should exist");

            // Build type parameter substitution map
            let mut type_subst: std::collections::HashMap<String, TypeId> =
                std::collections::HashMap::new();
            for (i, param) in base_variant.type_params.iter().enumerate() {
                if let Some(&type_arg) = type_args.get(i) {
                    type_subst.insert(param.name.clone(), type_arg);
                }
            }

            // Create mangled name for this instantiation
            let mangled_name = {
                let arg_names: Vec<String> = type_args
                    .iter()
                    .map(|&tid| type_table.mangle_type_name(tid))
                    .collect();
                format!("{}<{}>", base_name, arg_names.join(","))
            };

            // Skip if already registered (double-check)
            if self.variant_types.contains_key(&mangled_name) {
                continue;
            }

            // Define the base type with just the tag field
            let base_fields = vec![FieldType {
                element_type: StorageType::Val(ValType::I32),
                mutable: false,
            }];
            let base_type_idx = builder.define_gc_struct_type(&mangled_name, &base_fields);

            // Define each case as a subtype
            let mut case_infos = Vec::with_capacity(base_variant.cases.len());

            for case in &base_variant.cases {
                // Build fields for this case: tag + optional payload field
                let mut case_fields = vec![FieldType {
                    element_type: StorageType::Val(ValType::I32),
                    mutable: false,
                }];

                // Each variant case has exactly one payload type.
                // Substitute type parameters first.
                let concrete_type_id =
                    self.substitute_type_params(case.payload, &type_subst, type_table);
                let payload_is_unit =
                    matches!(type_table.get(concrete_type_id), ResolvedType::Unit);
                let payload_type = if payload_is_unit {
                    None
                } else {
                    let wasm_type = self.type_id_to_valtype(type_table, concrete_type_id);
                    case_fields.push(FieldType {
                        element_type: StorageType::Val(wasm_type),
                        mutable: true,
                    });
                    Some(wasm_type)
                };

                // Define the case subtype
                let case_type_name = format!("{}::{}", mangled_name, case.name);
                let case_type_idx =
                    builder.define_gc_struct_subtype(&case_type_name, base_type_idx, &case_fields);

                case_infos.push(VariantCaseInfo {
                    name: case.name.clone(),
                    type_idx: case_type_idx,
                    payload_type,
                });
            }

            // Store in registry
            self.variant_types.insert(
                mangled_name,
                VariantTypeInfo {
                    base_type_idx,
                    cases: case_infos,
                },
            );
        }
    }

    /// Register Result types from the type table.
    /// Result<T, E> is represented as a variant with Ok(T) and Err(E) cases using subtype hierarchy.
    fn register_result_types_from_table(
        &mut self,
        type_table: &TypeTable,
        builder: &mut CoreModuleBuilder,
    ) {
        // Collect all Result types from the type table
        let mut result_types: Vec<(TypeId, TypeId, TypeId)> = Vec::new(); // (type_id, ok, err)

        for type_id in type_table.iter_type_ids() {
            if let ResolvedType::Result { ok, err } = type_table.get(type_id) {
                let mangled_name = type_table.mangle_type_name(type_id);
                // Skip if already registered
                if !self.variant_types.contains_key(&mangled_name) {
                    result_types.push((type_id, *ok, *err));
                }
            }
        }

        // Register each Result type as a variant with subtype hierarchy
        for (type_id, ok_type_id, err_type_id) in result_types {
            let mangled_name = type_table.mangle_type_name(type_id);

            // Define the base type with just the tag field
            let base_fields = vec![FieldType {
                element_type: StorageType::Val(ValType::I32),
                mutable: false,
            }];
            let base_type_idx = builder.define_gc_struct_type(&mangled_name, &base_fields);

            // Determine the payload types
            let ok_type = self.type_id_to_valtype(type_table, ok_type_id);
            let err_type = self.type_id_to_valtype(type_table, err_type_id);

            // Define Ok case subtype
            let ok_case_name = format!("{mangled_name}::Ok");
            let mut ok_fields = vec![FieldType {
                element_type: StorageType::Val(ValType::I32),
                mutable: false,
            }];
            ok_fields.push(FieldType {
                element_type: StorageType::Val(ok_type),
                mutable: true,
            });
            let ok_type_idx =
                builder.define_gc_struct_subtype(&ok_case_name, base_type_idx, &ok_fields);

            // Define Err case subtype
            let err_case_name = format!("{mangled_name}::Err");
            let mut err_fields = vec![FieldType {
                element_type: StorageType::Val(ValType::I32),
                mutable: false,
            }];
            err_fields.push(FieldType {
                element_type: StorageType::Val(err_type),
                mutable: true,
            });
            let err_type_idx =
                builder.define_gc_struct_subtype(&err_case_name, base_type_idx, &err_fields);

            // Store in registry with Ok and Err cases
            self.variant_types.insert(
                mangled_name,
                VariantTypeInfo {
                    base_type_idx,
                    cases: vec![
                        VariantCaseInfo {
                            name: "Ok".to_string(),
                            type_idx: ok_type_idx,
                            payload_type: Some(ok_type),
                        },
                        VariantCaseInfo {
                            name: "Err".to_string(),
                            type_idx: err_type_idx,
                            payload_type: Some(err_type),
                        },
                    ],
                },
            );
        }
    }

    /// Substitute type parameters in a type ID.
    /// Returns the original type ID if no substitution is needed.
    fn substitute_type_params(
        &self,
        type_id: TypeId,
        subst: &std::collections::HashMap<String, TypeId>,
        type_table: &TypeTable,
    ) -> TypeId {
        match type_table.get(type_id) {
            ResolvedType::TypeParam { name, .. } => {
                // Substitute type parameter with concrete type
                subst.get(name).copied().unwrap_or(type_id)
            }
            _ => type_id, // No substitution needed
        }
    }

    /// Register box types for primitive references.
    /// Box types are single-field mutable structs that wrap primitive values,
    /// enabling references to primitives (e.g., `&i32`, `&mut f64`).
    fn register_box_types(&mut self, builder: &mut CoreModuleBuilder, project: &Project) {
        use PrimitiveType::{Bool, Char, F32, F64, I8, I16, I32, I64, U8, U16, U32, U64};

        // Check which ValTypes are needed based on used_box_primitives
        let needs_box_i32 = project
            .used_box_primitives
            .iter()
            .any(|p| matches!(p, I32 | I16 | I8 | U32 | U16 | U8 | Bool | Char));
        let needs_box_i64 = project
            .used_box_primitives
            .iter()
            .any(|p| matches!(p, I64 | U64));
        let needs_box_f32 = project.used_box_primitives.contains(&F32);
        let needs_box_f64 = project.used_box_primitives.contains(&F64);

        let primitives = [
            (ValType::I32, "$box_i32", needs_box_i32),
            (ValType::I64, "$box_i64", needs_box_i64),
            (ValType::F32, "$box_f32", needs_box_f32),
            (ValType::F64, "$box_f64", needs_box_f64),
        ];

        for (val_type, name, needed) in primitives {
            if !needed {
                continue;
            }
            let fields = vec![FieldType {
                element_type: StorageType::Val(val_type),
                mutable: true,
            }];
            let type_idx = builder.define_gc_struct_type(name, &fields);
            self.box_types.insert(val_type, type_idx);
        }
    }

    /// Get the box type index for a primitive `ValType`.
    /// Returns None if no box type is registered for this `ValType`.
    fn get_box_type_idx(&self, val_type: ValType) -> Option<u32> {
        self.box_types.get(&val_type).copied()
    }

    /// Get the tuple type index for a `TypeId` that is known to be a tuple.
    /// Returns None if the type is not a registered tuple.
    fn get_tuple_type_idx(&self, element_types: &[TypeId]) -> Option<u32> {
        self.tuple_types.get(element_types).copied()
    }

    /// Get the Wasm GC type index for a struct or tuple type.
    /// Handles reference types by looking through to the inner type.
    fn get_struct_or_tuple_type_idx(&self, type_id: TypeId, type_table: &TypeTable) -> u32 {
        match type_table.get(type_id) {
            ResolvedType::Struct {
                name,
                module_source,
                ..
            } => {
                if let Some(info) = self.lookup_struct_type(name, module_source) {
                    info.type_idx
                } else {
                    panic!("unknown struct type: {name}");
                }
            }
            ResolvedType::GenericInstance { .. } => {
                // All generic instances (including Array<T>) use the mangled name lookup
                let mangled_name = type_table.mangle_type_name(type_id);
                if let Some(info) =
                    self.lookup_struct_type(&mangled_name, &ModuleSource::entry_point())
                {
                    info.type_idx
                } else {
                    panic!("unknown generic struct type: {mangled_name}");
                }
            }
            ResolvedType::Tuple(elements) => {
                if let Some(type_idx) = self.get_tuple_type_idx(elements) {
                    type_idx
                } else {
                    panic!("unknown tuple type");
                }
            }
            // For references, look through to the inner type
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
                self.get_struct_or_tuple_type_idx(*inner, type_table)
            }
            // For newtypes, look through to the base type
            ResolvedType::Newtype { base_type, .. } => {
                self.get_struct_or_tuple_type_idx(*base_type, type_table)
            }
            other => {
                panic!("expected struct or tuple type, got: {other:?}");
            }
        }
    }

    /// Check if a type requires value copy (struct, array, tuple, string, option).
    /// Primitive types and references don't need copying.
    /// Empty tuples don't need copying (no fields to copy).
    /// Option<T> needs copying if T needs copying.
    fn needs_value_copy(&self, type_id: TypeId, type_table: &TypeTable) -> bool {
        match type_table.get(type_id) {
            ResolvedType::Struct { .. }
            | ResolvedType::GenericInstance { .. }
            | ResolvedType::Variant { .. } => true,
            ResolvedType::Tuple(elements) => !elements.is_empty(),
            ResolvedType::Option(inner) => self.needs_value_copy(*inner, type_table),
            _ => false,
        }
    }

    /// Check if a type is a reference type that uses GC references.
    /// Reference types need `ref.eq` for equality comparison instead of `i32.eq`.
    fn is_reference_type(&self, type_id: TypeId, type_table: &TypeTable) -> bool {
        match type_table.get(type_id) {
            ResolvedType::Struct { .. }
            | ResolvedType::GenericInstance { .. }
            | ResolvedType::Variant { .. }
            | ResolvedType::Ref(_)
            | ResolvedType::MutRef(_)
            | ResolvedType::Option(_)
            | ResolvedType::Function { .. } => true,
            ResolvedType::Tuple(elements) => !elements.is_empty(),
            _ => false,
        }
    }

    /// Generate code to copy a value for value semantics.
    /// Assumes the source value is on the stack. Leaves the copied value on the stack.
    fn generate_value_copy(
        &self,
        func: &mut Function,
        type_id: TypeId,
        type_table: &TypeTable,
        ctx: &mut FunctionContext,
        builder: &CoreModuleBuilder,
    ) {
        match type_table.get(type_id) {
            ResolvedType::Struct {
                name,
                module_source,
                ..
            } => {
                if let Some(info) = self.lookup_struct_type(name, module_source) {
                    self.generate_struct_copy(func, info.type_idx, info.field_count, ctx);
                } else {
                    panic!("unknown struct type: {name}");
                }
            }
            ResolvedType::Tuple(elements) => {
                let field_count = elements.len();
                if let Some(type_idx) = self.get_tuple_type_idx(elements) {
                    self.generate_struct_copy(func, type_idx, field_count, ctx);
                } else {
                    panic!("unknown tuple type");
                }
            }
            ResolvedType::GenericInstance {
                name, type_args, ..
            } if name == "Array" && type_args.len() == 1 => {
                let elem_type = type_args[0];
                if let Some(&raw_array_type_idx) = self.array_types.get(&elem_type) {
                    // Get the Array struct type
                    let array_struct_type_idx = self
                        .lookup_array_struct_type(elem_type, type_table)
                        .expect("Array struct type should be registered");
                    // Array is now a struct with (repr, used) fields
                    // 1. Store the source struct
                    let source_struct_name =
                        format!("__copy_array_struct_source_{raw_array_type_idx}");
                    let source_struct_local =
                        ctx.get_local(&source_struct_name).unwrap_or_else(|| {
                            ctx.alloc_local(
                                &source_struct_name,
                                ValType::Ref(RefType {
                                    nullable: true,
                                    heap_type: HeapType::Concrete(array_struct_type_idx),
                                }),
                            )
                        });
                    func.instruction(&Instruction::LocalSet(source_struct_local));

                    // 2. Get the repr field (raw array)
                    func.instruction(&Instruction::LocalGet(source_struct_local));
                    func.instruction(&Instruction::StructGet {
                        struct_type_index: array_struct_type_idx,
                        field_index: 0, // repr is field 0
                    });

                    // 3. Copy the raw array
                    // Check if element type is packed (i8/u8/i16/u16)
                    let elem_resolved = type_table.get(elem_type);
                    let is_packed = matches!(
                        elem_resolved,
                        ResolvedType::Primitive(
                            PrimitiveType::I8
                                | PrimitiveType::U8
                                | PrimitiveType::I16
                                | PrimitiveType::U16
                        )
                    );
                    self.generate_array_copy(func, raw_array_type_idx, is_packed, ctx);

                    // 4. Get the used field from source
                    func.instruction(&Instruction::LocalGet(source_struct_local));
                    func.instruction(&Instruction::StructGet {
                        struct_type_index: array_struct_type_idx,
                        field_index: 1, // used is field 1
                    });

                    // 5. Create new Array struct with (copied_repr, used)
                    func.instruction(&Instruction::StructNew(array_struct_type_idx));
                } else {
                    panic!("unknown array type");
                }
            }
            ResolvedType::Option(inner) => {
                // Option<T> where T needs copying: conditionally copy the inner value
                // Stack: [option_val]
                // If null, keep as-is. If not null, copy the inner value.
                if self.needs_value_copy(*inner, type_table) {
                    self.generate_option_copy(func, *inner, type_table, ctx, builder);
                }
                // If inner doesn't need copying, the option value is already on stack
            }
            ResolvedType::Variant { name, .. } => {
                // Variant types use subtype-based representation.
                // We need to check the tag and copy based on the specific case type.
                let variant_types = &self.variant_types;
                if let Some(info) = variant_types.get(name) {
                    let base_type_idx = info.base_type_idx;
                    let cases = info.cases.clone();
                    self.generate_variant_copy(func, base_type_idx, &cases, ctx);
                } else {
                    panic!("unknown variant type: {name}");
                }
            }
            _ => {
                // Primitives, references, etc. don't need copying
            }
        }
    }

    /// Generate code to copy a struct/tuple value.
    /// Assumes source struct reference is on the stack.
    /// Leaves the copied struct reference on the stack.
    fn generate_struct_copy(
        &self,
        func: &mut Function,
        type_idx: u32,
        field_count: usize,
        ctx: &mut FunctionContext,
    ) {
        // Use CopyContext to get the pre-allocated local
        let source_local = ctx
            .copy_context
            .get_struct_copy_local(type_idx)
            .unwrap_or_else(|| {
                panic!(
                    "BUG: struct copy local for type_idx {type_idx} not pre-allocated. \
                     This indicates a missing case in preallocate_value_copy_locals or \
                     CopyContext::expand_copy_types."
                )
            });

        // Store source to temp local (stack is now empty)
        func.instruction(&Instruction::LocalSet(source_local));

        // For each field, push the field value onto the stack
        for field_index in 0..field_count as u32 {
            func.instruction(&Instruction::LocalGet(source_local));
            func.instruction(&Instruction::StructGet {
                struct_type_index: type_idx,
                field_index,
            });
        }

        // Create a new struct with all field values
        func.instruction(&Instruction::StructNew(type_idx));
    }

    /// Generate code to copy a variant value with subtype-based representation.
    /// Assumes source variant reference is on the stack.
    /// Leaves the copied variant reference on the stack.
    ///
    /// Since each variant case has its own struct type, we need to:
    /// 1. Read the tag to determine which case it is
    /// 2. Cast to the appropriate case type
    /// 3. Copy the fields from that case
    /// 4. Create a new instance of the same case type
    fn generate_variant_copy(
        &self,
        func: &mut Function,
        base_type_idx: u32,
        cases: &[VariantCaseInfo],
        ctx: &mut FunctionContext,
    ) {
        // Use CopyContext to get the pre-allocated local for the base type
        let source_local = ctx
            .copy_context
            .get_struct_copy_local(base_type_idx)
            .unwrap_or_else(|| {
                panic!(
                    "BUG: variant copy local for base_type_idx {base_type_idx} not pre-allocated."
                )
            });

        // Store source to temp local
        func.instruction(&Instruction::LocalSet(source_local));

        // Generate a br_table to dispatch based on tag value
        // Each case copies its specific fields and creates a new instance
        //
        // Block structure:
        // block $done (result (ref $BaseType))
        //   block $case_N
        //     ...
        //     block $case_1
        //       block $case_0
        //         local.get $source
        //         struct.get $base 0  ;; get discriminator INSIDE blocks
        //         br_table $case_0 $case_1 ... $case_N $case_0
        //       end  ;; $case_0
        //       <copy case 0>
        //       br $done
        //     end  ;; $case_1
        //     <copy case 1>
        //     br $done
        //   end  ;; $case_N
        //   <copy case N>
        //   br $done (implicit)
        // end  ;; $done

        // Result type: nullable ref to base type
        let result_type = ValType::Ref(RefType {
            nullable: true,
            heap_type: HeapType::Concrete(base_type_idx),
        });

        // Start outer block ($done)
        func.instruction(&Instruction::Block(BlockType::Result(result_type)));

        // Create nested blocks for each case (innermost first in execution order)
        for _ in 0..cases.len() {
            func.instruction(&Instruction::Block(BlockType::Empty));
        }

        // Read tag from source INSIDE all blocks (field 0 is common to all cases)
        func.instruction(&Instruction::LocalGet(source_local));
        func.instruction(&Instruction::StructGet {
            struct_type_index: base_type_idx,
            field_index: 0,
        });

        // Generate br_table with case indices
        let targets: Vec<u32> = (0..cases.len() as u32).collect();
        func.instruction(&Instruction::BrTable(
            targets.clone().into(),
            targets.first().copied().unwrap_or(0), // default to case 0
        ));

        // Generate code for each case (in order of their blocks)
        // After ending each case block, we're one level less deep, so br depth decreases
        // Case 0: after End, depth to $done is cases.len() - 1
        // Case 1: after End, depth to $done is cases.len() - 2
        // etc.
        for (case_idx, case_info) in cases.iter().enumerate() {
            // End the current case's block
            func.instruction(&Instruction::End);

            // Copy this case: cast to case type, read all fields, create new struct
            let case_type_idx = case_info.type_idx;
            // tag + optional payload field
            let field_count = if case_info.payload_type.is_some() {
                2
            } else {
                1
            };

            // Read all fields and push onto stack
            // For each field: get source, cast to case type, read field
            // This duplicates the cast but avoids needing temp locals
            for field_index in 0..field_count as u32 {
                func.instruction(&Instruction::LocalGet(source_local));
                func.instruction(&Instruction::RefCastNonNull(HeapType::Concrete(
                    case_type_idx,
                )));
                func.instruction(&Instruction::StructGet {
                    struct_type_index: case_type_idx,
                    field_index,
                });
            }

            // Create new struct of this case type
            func.instruction(&Instruction::StructNew(case_type_idx));

            // Branch to $done block
            // After case_idx blocks have ended, we need to exit (cases.len() - 1 - case_idx) more
            let depth_to_done = (cases.len() - 1 - case_idx) as u32;
            func.instruction(&Instruction::Br(depth_to_done));
        }

        // End the outer $done block
        func.instruction(&Instruction::End);
    }

    /// Generate code to copy an array value.
    /// Assumes source array reference is on the stack.
    /// Leaves the copied array reference on the stack.
    /// `is_packed` should be true for arrays with packed storage (e.g., i8/i16 for strings).
    fn generate_array_copy(
        &self,
        func: &mut Function,
        array_type_idx: u32,
        is_packed: bool,
        ctx: &mut FunctionContext,
    ) {
        // Use CopyContext to get pre-allocated locals
        let locals = ctx
            .copy_context
            .get_array_copy_locals(array_type_idx)
            .unwrap_or_else(|| {
                panic!(
                    "BUG: array copy locals for type_idx {array_type_idx} not pre-allocated. \
                     This indicates a missing case in preallocate_value_copy_locals or \
                     CopyContext::expand_copy_types."
                )
            });
        let (source_local, dest_local, counter_local, len_local) =
            (locals.source, locals.dest, locals.counter, locals.len);

        // Store source to temp local
        func.instruction(&Instruction::LocalSet(source_local));

        // Get array length and store it
        func.instruction(&Instruction::LocalGet(source_local));
        func.instruction(&Instruction::ArrayLen);
        func.instruction(&Instruction::LocalSet(len_local));

        // Create destination array:
        // - If length == 0: create empty array with array.new_fixed 0
        // - If length > 0: use first element and array.new to create array of that length
        // Block structure for conditional:
        // block $done (result (ref $array))
        //   block $non_empty
        //     br_if $non_empty (len > 0)
        //     array.new_fixed 0
        //     br $done
        //   end
        //   ;; non-empty case: use first element
        //   source[0]
        //   len
        //   array.new
        // end

        // Result type for the blocks
        let array_ref_type = wasm_encoder::BlockType::Result(ValType::Ref(RefType {
            nullable: true,
            heap_type: HeapType::Concrete(array_type_idx),
        }));

        func.instruction(&Instruction::Block(array_ref_type)); // $done block
        func.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty)); // $non_empty block

        // Check if length > 0
        func.instruction(&Instruction::LocalGet(len_local));
        func.instruction(&Instruction::I32Const(0));
        func.instruction(&Instruction::I32GtU);
        func.instruction(&Instruction::BrIf(0)); // br $non_empty if len > 0

        // Empty case: create empty array
        func.instruction(&Instruction::ArrayNewFixed {
            array_type_index: array_type_idx,
            array_size: 0,
        });
        func.instruction(&Instruction::Br(1)); // br $done

        func.instruction(&Instruction::End); // end $non_empty

        // Non-empty case: use first element to create array
        func.instruction(&Instruction::LocalGet(source_local));
        func.instruction(&Instruction::I32Const(0));
        if is_packed {
            func.instruction(&Instruction::ArrayGetU(array_type_idx)); // get first element (packed)
        } else {
            func.instruction(&Instruction::ArrayGet(array_type_idx)); // get first element
        }
        func.instruction(&Instruction::LocalGet(len_local));
        func.instruction(&Instruction::ArrayNew(array_type_idx)); // create array filled with first element

        func.instruction(&Instruction::End); // end $done

        // Store destination array
        func.instruction(&Instruction::LocalSet(dest_local));

        // Copy loop: for i = 1; i < len; i++ (start from 1 since element 0 is already correct)
        // Initialize counter to 1
        func.instruction(&Instruction::I32Const(1));
        func.instruction(&Instruction::LocalSet(counter_local));

        // Loop start
        func.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
        func.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));

        // Check: counter < len
        func.instruction(&Instruction::LocalGet(counter_local));
        func.instruction(&Instruction::LocalGet(len_local));
        func.instruction(&Instruction::I32GeU);
        func.instruction(&Instruction::BrIf(1)); // Break if counter >= len

        // dest[counter] = source[counter]
        func.instruction(&Instruction::LocalGet(dest_local));
        func.instruction(&Instruction::RefAsNonNull); // array.set requires non-null ref
        func.instruction(&Instruction::LocalGet(counter_local));
        func.instruction(&Instruction::LocalGet(source_local));
        func.instruction(&Instruction::RefAsNonNull); // array.get requires non-null ref
        func.instruction(&Instruction::LocalGet(counter_local));
        if is_packed {
            func.instruction(&Instruction::ArrayGetU(array_type_idx));
        } else {
            func.instruction(&Instruction::ArrayGet(array_type_idx));
        }
        func.instruction(&Instruction::ArraySet(array_type_idx));

        // counter++
        func.instruction(&Instruction::LocalGet(counter_local));
        func.instruction(&Instruction::I32Const(1));
        func.instruction(&Instruction::I32Add);
        func.instruction(&Instruction::LocalSet(counter_local));

        // Continue loop
        func.instruction(&Instruction::Br(0));

        // End loop and block
        func.instruction(&Instruction::End); // End loop
        func.instruction(&Instruction::End); // End block

        // Push the destination array onto the stack
        func.instruction(&Instruction::LocalGet(dest_local));
    }

    /// Generate code to copy an Option<T> value where T needs copying.
    /// Assumes source option value is on the stack.
    /// Leaves the copied option value on the stack.
    fn generate_option_copy(
        &self,
        func: &mut Function,
        inner_type_id: TypeId,
        type_table: &TypeTable,
        ctx: &mut FunctionContext,
        builder: &CoreModuleBuilder,
    ) {
        // Get the Wasm type for the option (nullable ref)
        let inner_valtype = self.type_id_to_valtype(type_table, inner_type_id);
        let (option_valtype, inner_type_idx) = match inner_valtype {
            ValType::Ref(ref_type) => {
                let option_valtype = ValType::Ref(RefType {
                    nullable: true,
                    ..ref_type
                });
                let inner_type_idx = CopyContext::heap_type_to_idx(ref_type.heap_type);
                (option_valtype, inner_type_idx)
            }
            _ => {
                // For primitive inner types, option is boxed - but primitives don't need copying
                // This shouldn't happen since we check needs_value_copy first
                return;
            }
        };

        // Use CopyContext to get the pre-allocated local, keyed by inner type
        let inner_idx = inner_type_idx.unwrap_or_else(|| {
            panic!(
                "BUG: Option copy called for non-reference inner type. \
                 inner_type_id = {inner_type_id:?}"
            )
        });
        let source_local = ctx
            .copy_context
            .get_option_copy_local(inner_idx)
            .unwrap_or_else(|| {
                panic!(
                    "BUG: option copy local for inner_type_idx {inner_idx} not pre-allocated. \
                     This indicates a missing case in preallocate_value_copy_locals or \
                     CopyContext::expand_copy_types."
                )
            });

        // Store source to local
        func.instruction(&Instruction::LocalSet(source_local));

        // Block structure:
        // block $done (result option_type)
        //   block $is_null
        //     local.get source
        //     ref.is_null
        //     br_if $is_null  ; if null, jump to null handling
        //     ;; not null case: copy inner value
        //     local.get source
        //     ref.as_non_null
        //     <copy inner value>
        //     br $done
        //   end
        //   ;; null case
        //   ref.null
        // end

        let result_type = wasm_encoder::BlockType::Result(option_valtype);

        func.instruction(&Instruction::Block(result_type)); // $done
        func.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty)); // $is_null

        // Check if null
        func.instruction(&Instruction::LocalGet(source_local));
        func.instruction(&Instruction::RefIsNull);
        func.instruction(&Instruction::BrIf(0)); // br $is_null if null

        // Not null case: get the value and copy it
        func.instruction(&Instruction::LocalGet(source_local));
        func.instruction(&Instruction::RefAsNonNull);
        // Now we have the inner value on stack, copy it
        self.generate_value_copy(func, inner_type_id, type_table, ctx, builder);
        func.instruction(&Instruction::Br(1)); // br $done

        func.instruction(&Instruction::End); // end $is_null

        // Null case: push null
        if let ValType::Ref(ref_type) = option_valtype {
            func.instruction(&Instruction::RefNull(ref_type.heap_type));
        }

        func.instruction(&Instruction::End); // end $done
    }

    /// Check if a type contains an unregistered generic instance (user-defined generic struct).
    /// This is used to defer tuple registration until the generic struct is registered.
    fn contains_unregistered_generic_instance(
        &self,
        type_id: TypeId,
        type_table: &TypeTable,
    ) -> bool {
        match type_table.get(type_id) {
            ResolvedType::GenericInstance { .. } => {
                // Check if this generic instance is registered
                let mangled_name = type_table.mangle_type_name(type_id);
                !self
                    .struct_types
                    .contains_key(&StructName::new(ModuleSource::entry_point(), mangled_name))
            }
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
                self.contains_unregistered_generic_instance(*inner, type_table)
            }
            ResolvedType::Tuple(elements) => elements
                .iter()
                .any(|&elem| self.contains_unregistered_generic_instance(elem, type_table)),
            _ => false,
        }
    }

    /// Pre-register all tuple types found in a `TypeTable`.
    /// This must be called before code generation to ensure tuple types are available.
    fn register_tuple_types_from_table(
        &mut self,
        type_table: &TypeTable,
        builder: &mut CoreModuleBuilder,
    ) {
        for type_id in type_table.iter_type_ids() {
            if let ResolvedType::Tuple(elements) = type_table.get(type_id)
                && !elements.is_empty()
                && !self.tuple_types.contains_key(elements)
                // Skip tuples that contain type parameters (these are from generic templates)
                && !elements.iter().any(|&elem| type_table.contains_type_param(elem))
                // Skip tuples that contain unregistered generic instances
                && !elements.iter().any(|&elem| self.contains_unregistered_generic_instance(elem, type_table))
            {
                // Create the tuple type
                let mut fields = Vec::new();
                for &elem_type_id in elements {
                    let wasm_type = self.type_id_to_valtype(type_table, elem_type_id);
                    let storage_type = match wasm_type {
                        ValType::I32 => StorageType::Val(ValType::I32),
                        ValType::I64 => StorageType::Val(ValType::I64),
                        ValType::F32 => StorageType::Val(ValType::F32),
                        ValType::F64 => StorageType::Val(ValType::F64),
                        ValType::Ref(rt) => StorageType::Val(ValType::Ref(rt)),
                        _ => StorageType::Val(ValType::I32),
                    };
                    fields.push(FieldType {
                        element_type: storage_type,
                        mutable: true,
                    });
                }

                let type_name = format!("tuple_{}", elements.len());
                let type_idx = builder.define_gc_struct_type(&type_name, &fields);
                self.tuple_types.insert(elements.clone(), type_idx);
            }
        }
    }

    /// Generate a canonical name for an element type, used for deduplication.
    /// This handles the case where multiple `TypeIds` represent the same type.
    fn canonical_element_type_name(&self, type_id: TypeId, type_table: &TypeTable) -> String {
        match type_table.get(type_id) {
            ResolvedType::Primitive(p) => p.as_str().to_string(),
            ResolvedType::Struct { name, .. } => name.clone(),
            ResolvedType::GenericInstance {
                name, type_args, ..
            } => {
                let args: Vec<String> = type_args
                    .iter()
                    .map(|t| self.canonical_element_type_name(*t, type_table))
                    .collect();
                format!("{name}<{}>", args.join(","))
            }
            ResolvedType::Ref(inner) => {
                format!(
                    "ref_{}",
                    self.canonical_element_type_name(*inner, type_table)
                )
            }
            ResolvedType::MutRef(inner) => {
                format!(
                    "mutref_{}",
                    self.canonical_element_type_name(*inner, type_table)
                )
            }
            ResolvedType::Tuple(elems) => {
                let parts: Vec<String> = elems
                    .iter()
                    .map(|t| self.canonical_element_type_name(*t, type_table))
                    .collect();
                format!("tuple_{}", parts.join("_"))
            }
            ResolvedType::Option(inner) => {
                format!(
                    "option_{}",
                    self.canonical_element_type_name(*inner, type_table)
                )
            }
            ResolvedType::BuiltinArray(elem) => {
                format!(
                    "Array<{}>",
                    self.canonical_element_type_name(*elem, type_table)
                )
            }
            _ => format!("type_{type_id}"),
        }
    }

    /// Get or create an array type for a given element `TypeId`.
    /// Returns the Wasm type index for the GC array type.
    fn get_or_create_array_type(
        &mut self,
        element_type_id: TypeId,
        type_table: &TypeTable,
        builder: &mut CoreModuleBuilder,
    ) -> u32 {
        // Check if already registered by TypeId
        if let Some(&type_idx) = self.array_types.get(&element_type_id) {
            return type_idx;
        }

        // Check if already registered by name (handles duplicate TypeIds)
        let canonical_name = self.canonical_element_type_name(element_type_id, type_table);
        if let Some(&type_idx) = self.array_types_by_name.get(&canonical_name) {
            // Register the TypeId alias and return existing type
            self.array_types.insert(element_type_id, type_idx);
            return type_idx;
        }

        // Create new array type
        // Use packed storage types for i8/i16/u8/u16, otherwise use ValType
        // Use matches! on the actual type, not TypeId comparison,
        // because TypeIds may differ across modules
        let elem_resolved = type_table.get(element_type_id);
        let storage_type = if matches!(
            elem_resolved,
            ResolvedType::Primitive(PrimitiveType::I8 | PrimitiveType::U8)
        ) {
            StorageType::I8
        } else if matches!(
            elem_resolved,
            ResolvedType::Primitive(PrimitiveType::I16 | PrimitiveType::U16)
        ) {
            StorageType::I16
        } else {
            let wasm_type = self.type_id_to_valtype(type_table, element_type_id);
            match wasm_type {
                ValType::I32 => StorageType::Val(ValType::I32),
                ValType::I64 => StorageType::Val(ValType::I64),
                ValType::F32 => StorageType::Val(ValType::F32),
                ValType::F64 => StorageType::Val(ValType::F64),
                // For reference types, make them nullable so array.new_default works
                ValType::Ref(rt) => StorageType::Val(ValType::Ref(RefType {
                    nullable: true,
                    ..rt
                })),
                _ => StorageType::Val(ValType::I32),
            }
        };

        // Generate a type name based on element type
        let type_name = format!("array_{element_type_id}");
        let type_idx = builder.define_gc_array_type(&type_name, storage_type, true);

        self.array_types.insert(element_type_id, type_idx);
        self.array_types_by_name.insert(canonical_name, type_idx);
        type_idx
    }

    /// Get or create an Array<T> struct type for a given element `TypeId`.
    /// Array<T> is a struct with fields: repr (ref to GC array), used (i32)
    /// Returns the Wasm type index for the GC struct type.
    /// Note: Array<T> is now registered in `struct_types` like any other generic struct.
    fn get_or_create_array_struct_type(
        &mut self,
        element_type_id: TypeId,
        type_table: &TypeTable,
        builder: &mut CoreModuleBuilder,
    ) -> u32 {
        // Construct the mangled name for Array<T> (e.g., "Array<i32>")
        let element_name = type_table.mangle_type_name(element_type_id);
        let mangled_name = mangle_array_type(&element_name);

        // Check if already registered in struct_types
        if let Some(info) = self.lookup_struct_type(&mangled_name, &ModuleSource::entry_point()) {
            return info.type_idx;
        }

        // First ensure the raw array type exists
        let raw_array_type_idx =
            self.get_or_create_array_type(element_type_id, type_table, builder);

        // Create struct type with two fields:
        // - field 0: repr (ref to raw array, mutable for potential resize)
        //   Note: Must be nullable to match Wasm GC subtyping rules
        // - field 1: used (i32, mutable for append)
        let fields = vec![
            FieldType {
                element_type: StorageType::Val(ValType::Ref(RefType {
                    nullable: true,
                    heap_type: HeapType::Concrete(raw_array_type_idx),
                })),
                mutable: true,
            },
            FieldType {
                element_type: StorageType::Val(ValType::I32),
                mutable: true,
            },
        ];

        let type_name = mangle_generic_name("Array", &[element_name.clone()]);
        let type_idx = builder.define_gc_struct_type(&type_name, &fields);

        // Register in struct_types for consistency with other generic structs
        let struct_name = StructName::new(ModuleSource::entry_point(), mangled_name);
        self.struct_types.insert(
            struct_name,
            StructTypeInfo {
                type_idx,
                field_count: 2, // repr and used
                is_monomorphized: true,
                base_name: Some("Array".to_string()),
            },
        );
        type_idx
    }

    /// Look up an Array<T> struct type by element `TypeId`.
    /// Returns `Some(type_idx)` if registered, None otherwise.
    fn lookup_array_struct_type(
        &self,
        element_type_id: TypeId,
        type_table: &TypeTable,
    ) -> Option<u32> {
        let element_name = type_table.mangle_type_name(element_type_id);
        let mangled_name = mangle_array_type(&element_name);
        self.lookup_struct_type(&mangled_name, &ModuleSource::entry_point())
            .map(|info| info.type_idx)
    }

    /// Get the next unique closure ID
    fn get_next_closure_id(&mut self) -> u32 {
        let id = self.closure_counter;
        self.closure_counter += 1;
        id
    }

    /// Get or create a canonical closure type for a function signature.
    /// This is used for function type parameters (e.g., `fn(i32) -> i32`).
    /// Returns (`canonical_fn_type_idx`, `canonical_fn_type_name`, `canonical_closure_struct_type_idx`).
    ///
    /// The canonical closure uses `(ref struct)` as the environment type,
    /// allowing any closure with the same user-visible signature to be compatible.
    fn get_or_create_canonical_closure_type(
        &mut self,
        params: &[TypeId],
        return_type: TypeId,
        type_table: &TypeTable,
        builder: &mut CoreModuleBuilder,
    ) -> (u32, String, u32) {
        let key = (params.to_vec(), return_type);

        // Check if already registered
        if let Some((fn_type_idx, fn_type_name, struct_type_idx)) =
            self.canonical_closure_types.get(&key).cloned()
        {
            return (fn_type_idx, fn_type_name, struct_type_idx);
        }

        let closure_id = self.get_next_closure_id();

        // Create canonical function type with generic struct ref as first param
        let mut fn_param_types = vec![ValType::Ref(RefType {
            nullable: false,
            heap_type: HeapType::Abstract {
                shared: false,
                ty: AbstractHeapType::Struct,
            },
        })];
        for type_id in params {
            fn_param_types.push(self.type_id_to_valtype(type_table, *type_id));
        }

        let fn_return_types: Vec<ValType> =
            if return_type == TypeTable::NEVER || return_type == TypeTable::UNIT {
                vec![]
            } else {
                vec![self.type_id_to_valtype(type_table, return_type)]
            };

        let fn_type_name = format!("$canonical_closure_fn_{closure_id}");
        builder.define_func_type(&fn_type_name, &fn_param_types, &fn_return_types);
        let fn_type_idx = builder.type_idx(&fn_type_name);

        // Create canonical closure struct type (generic env + funcref)
        let struct_type_name = format!("CanonicalClosure_{closure_id}");
        let fields = vec![
            FieldType {
                element_type: StorageType::Val(ValType::Ref(RefType {
                    nullable: false,
                    heap_type: HeapType::Abstract {
                        shared: false,
                        ty: AbstractHeapType::Struct,
                    },
                })),
                mutable: false,
            },
            FieldType {
                element_type: StorageType::Val(ValType::Ref(RefType {
                    nullable: false,
                    heap_type: HeapType::Concrete(fn_type_idx),
                })),
                mutable: false,
            },
        ];

        let struct_type_idx = builder.define_gc_struct_type(&struct_type_name, &fields);

        self.canonical_closure_types
            .insert(key, (fn_type_idx, fn_type_name.clone(), struct_type_idx));

        (fn_type_idx, fn_type_name, struct_type_idx)
    }

    /// Pre-register primitive array types (where element type is a primitive).
    /// These can be registered before struct types since they don't depend on struct definitions.
    fn register_primitive_array_types_from_table(
        &mut self,
        type_table: &TypeTable,
        builder: &mut CoreModuleBuilder,
    ) {
        for type_id in type_table.iter_type_ids() {
            let (element_type_id, is_array_struct) =
                if let Some(elem) = type_table.as_array(type_id) {
                    (elem, true)
                } else if let ResolvedType::BuiltinArray(elem) = type_table.get(type_id) {
                    (*elem, false)
                } else {
                    continue;
                };
            // Skip if element type is not a primitive
            // Note: String is handled later in register_array_types_from_table because
            // it requires the String struct to be registered first
            if !matches!(type_table.get(element_type_id), ResolvedType::Primitive(_)) {
                continue;
            }
            // Skip array types with type parameters (unmonomorphized generics)
            if type_table.contains_type_param(element_type_id) {
                continue;
            }
            // Register raw array type
            if !self.array_types.contains_key(&element_type_id) {
                self.get_or_create_array_type(element_type_id, type_table, builder);
            }
            // Also register Array struct type for Array<T>
            if is_array_struct
                && self
                    .lookup_array_struct_type(element_type_id, type_table)
                    .is_none()
            {
                self.get_or_create_array_struct_type(element_type_id, type_table, builder);
            }
        }
    }

    /// Pre-register arrays of non-monomorphized structs.
    /// This includes arrays like Array<String> where String is a non-monomorphized struct.
    /// Must be called after non-monomorphized structs are registered (PHASE 1-2)
    /// and before monomorphized structs are registered (PHASE 3-4).
    fn register_non_monomorphized_struct_arrays(
        &mut self,
        type_table: &TypeTable,
        builder: &mut CoreModuleBuilder,
    ) {
        for type_id in type_table.iter_type_ids() {
            let (element_type_id, is_array_struct) =
                if let Some(elem) = type_table.as_array(type_id) {
                    (elem, true)
                } else if let ResolvedType::BuiltinArray(elem) = type_table.get(type_id) {
                    (*elem, false)
                } else {
                    continue;
                };

            // Only process non-monomorphized struct element types
            let is_non_monomorphized_struct = match type_table.get(element_type_id) {
                ResolvedType::Struct {
                    is_monomorphized, ..
                } => !is_monomorphized,
                _ => false,
            };
            if !is_non_monomorphized_struct {
                continue;
            }

            // Skip array types with type parameters (unmonomorphized generics)
            if type_table.contains_type_param(element_type_id) {
                continue;
            }

            // Register raw array type (for builtin::array<T>)
            if !self.array_types.contains_key(&element_type_id) {
                self.get_or_create_array_type(element_type_id, type_table, builder);
            }

            // Also register Array struct type (for Array<T>)
            if is_array_struct
                && self
                    .lookup_array_struct_type(element_type_id, type_table)
                    .is_none()
            {
                self.get_or_create_array_struct_type(element_type_id, type_table, builder);
            }
        }
    }

    /// Ensure all `GenericInstance` types needed by a field type are registered.
    /// This handles cases where:
    /// - A `GenericInstance` is excluded from topological sort (e.g., Array types
    ///   which are filtered out from monomorphized struct registration)
    /// - Nested `GenericInstance` types need registration (e.g., `Array<Box<i32>>`)
    ///
    /// Recursively handles nested types like `Option<T>`, `&T`, tuples, etc.
    fn ensure_field_generic_types_registered(
        &mut self,
        type_id: TypeId,
        type_table: &TypeTable,
        builder: &mut CoreModuleBuilder,
    ) {
        match type_table.get(type_id) {
            ResolvedType::GenericInstance {
                name, type_args, ..
            } => {
                // First, recursively ensure type arguments are registered
                for type_arg in type_args {
                    self.ensure_field_generic_types_registered(*type_arg, type_table, builder);
                }

                // Check if this GenericInstance is already registered in struct_types
                let mangled_name = type_table.mangle_type_name(type_id);
                let is_registered_in_struct_types = self
                    .lookup_struct_type(&mangled_name, &ModuleSource::entry_point())
                    .is_some();

                // If not in struct_types and it's an Array type, register it.
                // Note: Array<T> is now treated like any other generic container and uses struct_types.
                if !is_registered_in_struct_types && name == "Array" && type_args.len() == 1 {
                    let element_type_id = type_args[0];
                    self.get_or_create_array_struct_type(element_type_id, type_table, builder);
                }
            }
            // Recurse into container types
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
                self.ensure_field_generic_types_registered(*inner, type_table, builder);
            }
            ResolvedType::Option(inner) => {
                self.ensure_field_generic_types_registered(*inner, type_table, builder);
            }
            ResolvedType::Tuple(elements) => {
                for elem in elements {
                    self.ensure_field_generic_types_registered(*elem, type_table, builder);
                }
            }
            ResolvedType::Result { ok, err } => {
                self.ensure_field_generic_types_registered(*ok, type_table, builder);
                self.ensure_field_generic_types_registered(*err, type_table, builder);
            }
            _ => {}
        }
    }

    /// Pre-register array types from monomorphized struct fields.
    /// This is needed BEFORE monomorphized structs are registered, so that fields
    /// with Array<Tuple<...>> or other non-primitive element types can be properly typed.
    fn pre_register_arrays_from_monomorphized_structs(
        &mut self,
        tir_module: &TirModule,
        type_table: &TypeTable,
        builder: &mut CoreModuleBuilder,
    ) {
        // Find all monomorphized structs (non-Array)
        let mono_structs: Vec<_> = tir_module
            .structs
            .iter()
            .filter(|s| {
                s.type_params.is_empty()
                    && s.monomorph_info.is_some()
                    && s.monomorph_info
                        .as_ref()
                        .map(|i| i.generic_name != "Array")
                        .unwrap_or(true)
            })
            .collect();

        // For each monomorphized struct, scan field types for Array<T> instances
        let mut array_types_to_register: Vec<(TypeId, bool)> = Vec::new();
        for tir_struct in mono_structs {
            for field in &tir_struct.fields {
                self.collect_array_types_recursive(
                    field.type_id,
                    type_table,
                    &mut array_types_to_register,
                    &mut std::collections::HashSet::new(),
                );
            }
        }

        // Register the discovered array types
        for (element_type_id, is_array_struct) in array_types_to_register {
            // Skip array types with type parameters (unmonomorphized generics)
            if type_table.contains_type_param(element_type_id) {
                continue;
            }
            // Skip array types with Unknown/Error element types
            if element_type_id == TypeTable::UNKNOWN || element_type_id == TypeTable::ERROR {
                continue;
            }
            // Skip element types that involve monomorphized structs (GenericInstance)
            // These are handled by either:
            // - Self-referential struct registration with rec groups
            // - Later PHASE 5 array registration after structs are registered
            if self.element_type_involves_unregistered_struct(element_type_id, type_table) {
                continue;
            }
            // Register raw array type
            if !self.array_types.contains_key(&element_type_id) {
                self.get_or_create_array_type(element_type_id, type_table, builder);
            }
            // Also register Array struct type
            if is_array_struct
                && self
                    .lookup_array_struct_type(element_type_id, type_table)
                    .is_none()
            {
                self.get_or_create_array_struct_type(element_type_id, type_table, builder);
            }
        }
    }

    /// Check if an element type involves an unregistered monomorphized struct
    /// (`GenericInstance` or Struct with mangled name like "Foo<T>").
    /// This recursively checks nested types.
    fn element_type_involves_unregistered_struct(
        &self,
        type_id: TypeId,
        type_table: &TypeTable,
    ) -> bool {
        match type_table.get(type_id) {
            ResolvedType::GenericInstance { .. } => true,
            // Check for monomorphized struct types that aren't registered yet in struct_types
            ResolvedType::Struct {
                name,
                module_source,
                is_monomorphized: true,
                ..
            } => {
                let struct_name = StructName::new(module_source.clone(), name.clone());
                !self.struct_types.contains_key(&struct_name)
            }
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
                self.element_type_involves_unregistered_struct(*inner, type_table)
            }
            ResolvedType::Option(inner) => {
                self.element_type_involves_unregistered_struct(*inner, type_table)
            }
            ResolvedType::Tuple(elements) => elements
                .iter()
                .any(|e| self.element_type_involves_unregistered_struct(*e, type_table)),
            ResolvedType::Result { ok, err } => {
                self.element_type_involves_unregistered_struct(*ok, type_table)
                    || self.element_type_involves_unregistered_struct(*err, type_table)
            }
            _ => false,
        }
    }

    /// Pre-register all array types found in a `TypeTable`.
    /// Registers both raw array types (for `builtin::array`<T>) and Array struct types (for Array<T>).
    fn register_array_types_from_table(
        &mut self,
        type_table: &TypeTable,
        builder: &mut CoreModuleBuilder,
    ) {
        // Collect all array types from the type table, including nested ones
        let mut array_types_to_register: Vec<(TypeId, bool)> = Vec::new();
        for type_id in type_table.iter_type_ids() {
            self.collect_array_types_recursive(
                type_id,
                type_table,
                &mut array_types_to_register,
                &mut std::collections::HashSet::new(),
            );
        }

        // Register all discovered array types
        for (element_type_id, is_array_struct) in array_types_to_register {
            // Skip array types with type parameters (unmonomorphized generics)
            if type_table.contains_type_param(element_type_id) {
                continue;
            }
            // Skip array types with Unknown/Error element types
            if element_type_id == TypeTable::UNKNOWN || element_type_id == TypeTable::ERROR {
                continue;
            }
            // Note: We no longer skip GenericInstance element types here.
            // Fully concrete GenericInstances like Pair<i32, String> are valid
            // element types for arrays. The contains_type_param check above
            // already handles unmonomorphized generics.
            // Register raw array type (for builtin::array<T> and Array<T>.repr)
            if !self.array_types.contains_key(&element_type_id) {
                self.get_or_create_array_type(element_type_id, type_table, builder);
            }
            // Also register Array struct type for Array<T>
            if is_array_struct
                && self
                    .lookup_array_struct_type(element_type_id, type_table)
                    .is_none()
            {
                self.get_or_create_array_struct_type(element_type_id, type_table, builder);
            }
        }
    }

    /// Pre-register canonical closure types for all function types found in the type table.
    /// This is needed so that function type parameters can be properly typed.
    fn register_canonical_closure_types_from_table(
        &mut self,
        type_table: &TypeTable,
        builder: &mut CoreModuleBuilder,
    ) {
        for type_id in type_table.iter_type_ids() {
            if let ResolvedType::Function {
                params,
                return_type,
                ..
            } = type_table.get(type_id)
            {
                // Skip if any param or return type contains type parameters (unmonomorphized)
                let has_type_params = params.iter().any(|p| type_table.contains_type_param(*p))
                    || type_table.contains_type_param(*return_type);
                if has_type_params {
                    continue;
                }

                // Register canonical closure type for this function signature
                self.get_or_create_canonical_closure_type(
                    params,
                    *return_type,
                    type_table,
                    builder,
                );
            }
        }
    }

    /// Recursively collect array types from a type, including nested types in structs/tuples
    fn collect_array_types_recursive(
        &self,
        type_id: TypeId,
        type_table: &TypeTable,
        found: &mut Vec<(TypeId, bool)>,
        visited: &mut std::collections::HashSet<TypeId>,
    ) {
        if !visited.insert(type_id) {
            return; // Already visited
        }

        match type_table.get(type_id) {
            ResolvedType::GenericInstance {
                name, type_args, ..
            } if name == "Array" && type_args.len() == 1 => {
                let elem = type_args[0];
                found.push((elem, true));
                self.collect_array_types_recursive(elem, type_table, found, visited);
            }
            ResolvedType::GenericInstance { type_args, .. } => {
                // Recurse into type arguments for other generic instances
                for arg in type_args {
                    self.collect_array_types_recursive(*arg, type_table, found, visited);
                }
            }
            ResolvedType::BuiltinArray(elem) => {
                found.push((*elem, false));
                self.collect_array_types_recursive(*elem, type_table, found, visited);
            }
            ResolvedType::Struct { .. } => {
                // Struct field types are stored in TirStruct, not ResolvedType::Struct
                // We handle nested arrays by scanning all types in the table
            }
            ResolvedType::Tuple(elements) => {
                for elem in elements {
                    self.collect_array_types_recursive(*elem, type_table, found, visited);
                }
            }
            ResolvedType::Option(inner)
            | ResolvedType::Ref(inner)
            | ResolvedType::MutRef(inner) => {
                self.collect_array_types_recursive(*inner, type_table, found, visited);
            }
            _ => {}
        }
    }

    /// Convert a global variable initializer to a Wasm constant expression
    /// Only supports constant expressions (literals, null)
    fn global_init_to_const_expr(init: &TirExpr, type_table: &TypeTable) -> ConstExpr {
        use wasm_encoder::{Ieee32, Ieee64};

        match &init.kind {
            TirExprKind::IntLiteral { value, .. } => {
                // Determine the right type of constant based on the expression type
                // Follow newtype chain to get the primitive type
                let base_type = type_table.get_ultimate_base_type(init.type_id);
                match type_table.get(base_type) {
                    ResolvedType::Primitive(prim) => match prim {
                        PrimitiveType::I8
                        | PrimitiveType::I16
                        | PrimitiveType::I32
                        | PrimitiveType::U8
                        | PrimitiveType::U16
                        | PrimitiveType::U32 => ConstExpr::i32_const(*value as i32),
                        PrimitiveType::I64 | PrimitiveType::U64 => {
                            ConstExpr::i64_const(*value as i64)
                        }
                        _ => panic!(
                            "unexpected primitive type for int literal: {:?}",
                            type_table.get(init.type_id)
                        ),
                    },
                    _ => ConstExpr::i32_const(*value as i32), // Default to i32
                }
            }
            TirExprKind::FloatLiteral { value, .. } => {
                // Follow newtype chain to get the primitive type
                let base_type = type_table.get_ultimate_base_type(init.type_id);
                match type_table.get(base_type) {
                    ResolvedType::Primitive(PrimitiveType::F32) => {
                        ConstExpr::f32_const(Ieee32::from(*value as f32))
                    }
                    ResolvedType::Primitive(PrimitiveType::F64) => {
                        ConstExpr::f64_const(Ieee64::from(*value))
                    }
                    _ => ConstExpr::f64_const(Ieee64::from(*value)), // Default to f64
                }
            }
            TirExprKind::BoolLiteral(b) => ConstExpr::i32_const(i32::from(*b)),
            TirExprKind::Null => {
                // For null, we need a ref.null of the appropriate type
                // For Option<T>, null means None
                ConstExpr::ref_null(HeapType::Abstract {
                    shared: false,
                    ty: AbstractHeapType::None,
                })
            }
            TirExprKind::Unit => {
                // Unit type - use 0
                ConstExpr::i32_const(0)
            }
            TirExprKind::Cast { expr: inner, .. } => {
                // For casts, evaluate the inner expression with the cast's target type
                // Create a synthetic TirExpr with the inner expression but outer type
                let typed_inner = TirExpr::new(inner.kind.clone(), init.type_id, init.span);
                Self::global_init_to_const_expr(&typed_inner, type_table)
            }
            _ => {
                // For non-constant initializers, use null as placeholder
                // The actual initialization happens in __initialize_globals
                ConstExpr::ref_null(HeapType::Abstract {
                    shared: false,
                    ty: AbstractHeapType::None,
                })
            }
        }
    }

    /// Convert TIR `TypeId` to Wasm `ValType`
    fn type_id_to_valtype(&self, type_table: &TypeTable, type_id: TypeId) -> ValType {
        match type_table.get(type_id) {
            // Primitive types
            ResolvedType::Primitive(prim) => match prim {
                PrimitiveType::I8
                | PrimitiveType::I16
                | PrimitiveType::I32
                | PrimitiveType::U8
                | PrimitiveType::U16
                | PrimitiveType::U32 => ValType::I32,
                PrimitiveType::I64 | PrimitiveType::U64 => ValType::I64,
                PrimitiveType::I128 | PrimitiveType::U128 => {
                    // TODO: i128/u128 need special handling (tuple of i64s)
                    panic!("i128/u128 not yet supported in TIR codegen")
                }
                PrimitiveType::F32 => ValType::F32,
                PrimitiveType::F64 => ValType::F64,
                PrimitiveType::Bool | PrimitiveType::Char => ValType::I32,
            },

            // Unit type - no value on stack
            ResolvedType::Unit => ValType::I32, // Placeholder, unit is typically elided

            // Never type - functions that never return
            ResolvedType::Never => ValType::I32, // Placeholder, never actually used

            // Struct type
            ResolvedType::Struct {
                name,
                module_source,
                ..
            } => {
                // Check pending_type_indices first (for rec group construction)
                if let Some(&type_idx) = self.pending_type_indices.get(name) {
                    return ValType::Ref(RefType {
                        nullable: false,
                        heap_type: HeapType::Concrete(type_idx),
                    });
                }
                // Special case: String struct - always use the canonical module source
                let lookup_source = if name == "String" {
                    string_module_source()
                } else {
                    module_source.clone()
                };
                if let Some(struct_info) = self.lookup_struct_type(name, &lookup_source) {
                    ValType::Ref(RefType {
                        nullable: false,
                        heap_type: HeapType::Concrete(struct_info.type_idx),
                    })
                } else {
                    panic!("unknown struct type in type_id_to_valtype: {name}")
                }
            }

            // Array<T> - GC struct with repr (raw array) and used (i32) fields
            // Now treated like any other generic struct via struct_types lookup
            ResolvedType::GenericInstance {
                name, type_args, ..
            } if name == "Array" && type_args.len() == 1 => {
                let element_type = type_args[0];
                // Look up registered Array struct type
                if let Some(type_idx) = self.lookup_array_struct_type(element_type, type_table) {
                    ValType::Ref(RefType {
                        nullable: false,
                        heap_type: HeapType::Concrete(type_idx),
                    })
                } else {
                    // Array type not yet registered - this can happen with recursive types
                    // (e.g., struct BTreeNode { children: Array<&mut BTreeNode> })
                    // Use nullable structref as a fallback for now
                    // TODO: Properly handle recursive types with Wasm GC rec groups
                    ValType::Ref(RefType {
                        nullable: true,
                        heap_type: HeapType::Abstract {
                            shared: false,
                            ty: AbstractHeapType::Struct,
                        },
                    })
                }
            }

            // builtin::array<T> - raw GC array intrinsic
            // Note: Must be nullable to match Wasm GC subtyping rules when used in struct fields
            ResolvedType::BuiltinArray(element_type) => {
                // Look up registered raw array type
                let type_idx = self
                    .array_types
                    .get(element_type)
                    .copied()
                    .or_else(|| self.array_types.get(&TypeTable::U8).copied())
                    .expect("array type should be registered");
                ValType::Ref(RefType {
                    nullable: true,
                    heap_type: HeapType::Concrete(type_idx),
                })
            }

            // Option<T> - nullable reference
            ResolvedType::Option(inner) => {
                // For primitive types, use nullable box reference
                if let ResolvedType::Primitive(prim) = type_table.get(*inner) {
                    let val_type = primitive_to_valtype(prim);
                    if let Some(box_type_idx) = self.get_box_type_idx(val_type) {
                        return ValType::Ref(RefType {
                            nullable: true,
                            heap_type: HeapType::Concrete(box_type_idx),
                        });
                    }
                }
                // For reference types, make the reference nullable
                let inner_valtype = self.type_id_to_valtype(type_table, *inner);
                match inner_valtype {
                    ValType::Ref(ref_type) => ValType::Ref(RefType {
                        nullable: true,
                        ..ref_type
                    }),
                    // Fallback for edge cases (shouldn't happen with primitives handled above)
                    _ => inner_valtype,
                }
            }

            // Reference types
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
                // For primitive references (including newtypes of primitives), use the box type
                // Follow newtypes to find the ultimate primitive base type
                let ultimate_inner = type_table.get_ultimate_base_type(*inner);
                if let ResolvedType::Primitive(prim) = type_table.get(ultimate_inner) {
                    let val_type = primitive_to_valtype(prim);
                    if let Some(box_type_idx) = self.get_box_type_idx(val_type) {
                        return ValType::Ref(RefType {
                            nullable: false,
                            heap_type: HeapType::Concrete(box_type_idx),
                        });
                    }
                }
                // For non-primitive references (structs, arrays, etc.), pass through
                self.type_id_to_valtype(type_table, *inner)
            }

            // Function type - look up canonical closure struct type
            ResolvedType::Function {
                params,
                return_type,
                ..
            } => {
                let key = (params.clone(), *return_type);
                if let Some((_, _, struct_type_idx)) =
                    self.canonical_closure_types.get(&key).cloned()
                {
                    ValType::Ref(RefType {
                        nullable: false,
                        heap_type: HeapType::Concrete(struct_type_idx),
                    })
                } else {
                    // Type not yet registered - return placeholder struct ref
                    ValType::Ref(RefType {
                        nullable: false,
                        heap_type: HeapType::Abstract {
                            shared: false,
                            ty: AbstractHeapType::Struct,
                        },
                    })
                }
            }

            // Tuple type
            ResolvedType::Tuple(elements) => {
                if elements.is_empty() {
                    ValType::I32 // Empty tuple represented as i32(0)
                } else if let Some(&type_idx) = self.tuple_types.get(elements) {
                    // Use registered tuple type
                    ValType::Ref(RefType {
                        nullable: false,
                        heap_type: HeapType::Concrete(type_idx),
                    })
                } else {
                    // Tuple type not yet registered - return placeholder.
                    // The actual type will be created during expression codegen.
                    ValType::Ref(RefType {
                        nullable: false,
                        heap_type: HeapType::Abstract {
                            shared: false,
                            ty: AbstractHeapType::Struct,
                        },
                    })
                }
            }

            // Complex types that need special handling
            ResolvedType::Enum { .. }
            | ResolvedType::Resource { .. } // Resource handles are i32 (CM resource handles)
            | ResolvedType::Stream(_)
            | ResolvedType::Future(_)
            | ResolvedType::Reactive(_) => {
                // TODO: Implement proper handling for these types
                // Use i32 as placeholder for now
                ValType::I32
            }

            // Types not yet implemented
            ResolvedType::Result { .. } => {
                panic!("Result type codegen not yet implemented")
            }
            ResolvedType::Variant { name, .. } => {
                // Check pending_type_indices first (for rec group construction)
                if let Some(&type_idx) = self.pending_type_indices.get(name) {
                    return ValType::Ref(RefType {
                        nullable: true,
                        heap_type: HeapType::Concrete(type_idx),
                    });
                }
                // Custom variant types are represented as GC struct references (base type)
                let variant_types = &self.variant_types;
                if let Some(info) = variant_types.get(name) {
                    ValType::Ref(RefType {
                        nullable: true,
                        heap_type: HeapType::Concrete(info.base_type_idx),
                    })
                } else {
                    panic!("Variant type not registered: {name}");
                }
            }
            // Placeholder types (shouldn't appear in final TIR)
            ResolvedType::Unknown | ResolvedType::Error => {
                panic!("unexpected Unknown/Error type in codegen")
            }

            // Type parameters should be monomorphized before codegen
            ResolvedType::TypeParam { name, .. } => {
                panic!("type parameter '{name}' should be monomorphized before codegen")
            }

            // Generic instances (other than Array, which is handled above)
            // Look up the monomorphized struct or variant type
            ResolvedType::GenericInstance { .. } => {
                let mangled_name = type_table.mangle_type_name(type_id);
                // Check pending_type_indices first (for rec group construction)
                if let Some(&type_idx) = self.pending_type_indices.get(&mangled_name) {
                    return ValType::Ref(RefType {
                        nullable: false,
                        heap_type: HeapType::Concrete(type_idx),
                    });
                }
                if let Some(struct_info) =
                    self.lookup_struct_type(&mangled_name, &ModuleSource::entry_point())
                {
                    ValType::Ref(RefType {
                        nullable: false,
                        heap_type: HeapType::Concrete(struct_info.type_idx),
                    })
                } else if let Some(variant_info) = self.variant_types.get(&mangled_name) {
                    // Generic variant (like Result<i32, String>) - use base type
                    ValType::Ref(RefType {
                        nullable: true,
                        heap_type: HeapType::Concrete(variant_info.base_type_idx),
                    })
                } else {
                    panic!(
                        "unknown monomorphized generic type in type_id_to_valtype: {mangled_name}"
                    )
                }
            }

            // Newtype: same representation as base type
            ResolvedType::Newtype { base_type, .. } => {
                self.type_id_to_valtype(type_table, *base_type)
            }
        }
    }

    /// Convert a type to `ValType`, using a forward reference for self-referential struct types.
    /// This is used when registering recursive structs in a rec group.
    fn type_id_to_valtype_with_self_ref(
        type_table: &TypeTable,
        type_id: TypeId,
        self_type_idx: u32,
    ) -> ValType {
        match type_table.get(type_id) {
            // For Struct and GenericInstance that would normally require lookup,
            // use the forward reference since this IS the struct being defined
            ResolvedType::Struct { .. } | ResolvedType::GenericInstance { .. } => {
                ValType::Ref(RefType {
                    nullable: false,
                    heap_type: HeapType::Concrete(self_type_idx),
                })
            }
            // Option<T> - make the inner type nullable
            ResolvedType::Option(inner) => {
                let inner_valtype =
                    Self::type_id_to_valtype_with_self_ref(type_table, *inner, self_type_idx);
                match inner_valtype {
                    ValType::Ref(ref_type) => ValType::Ref(RefType {
                        nullable: true,
                        ..ref_type
                    }),
                    other => other,
                }
            }
            // Ref/MutRef - recurse into inner
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
                Self::type_id_to_valtype_with_self_ref(type_table, *inner, self_type_idx)
            }
            // For other types, use standard conversion (they don't reference self)
            ResolvedType::Primitive(prim) => primitive_to_valtype(prim),
            ResolvedType::Unit => ValType::I32,
            _ => ValType::I32, // Fallback
        }
    }

    /// Check if a type is a reference type (struct, string, variant, etc.)
    /// These types are represented as GC references in Wasm.
    fn type_is_reference(&self, type_id: TypeId, type_table: &TypeTable) -> bool {
        matches!(
            type_table.get(type_id),
            ResolvedType::Struct { .. }
                | ResolvedType::GenericInstance { .. }
                | ResolvedType::Tuple(_)
                | ResolvedType::Variant { .. }
                | ResolvedType::Ref(_)
                | ResolvedType::MutRef(_)
                | ResolvedType::Option(_)
                | ResolvedType::Function { .. }
        )
    }

    // ========================================================================
    // Code Generation
    // ========================================================================

    /// Generate code for a TIR expression
    fn generate_expr(
        &self,
        func: &mut Function,
        expr: &TirExpr,
        type_table: &TypeTable,
        ctx: &mut FunctionContext,
        builder: &CoreModuleBuilder,
    ) {
        match &expr.kind {
            // === Literals ===
            TirExprKind::IntLiteral { value, .. } => {
                // Check the target type to generate appropriate instruction
                // Follow newtype chain to get the primitive type
                let base_type = type_table.get_ultimate_base_type(expr.type_id);
                // Reinterpret u64 bits as i64 for Wasm instruction
                match type_table.get(base_type) {
                    ResolvedType::Primitive(PrimitiveType::I64 | PrimitiveType::U64) => {
                        func.instruction(&Instruction::I64Const(*value as i64));
                    }
                    _ => {
                        func.instruction(&Instruction::I32Const(*value as i32));
                    }
                }
            }

            TirExprKind::FloatLiteral { value, .. } => {
                // Follow newtype chain to get the primitive type
                let base_type = type_table.get_ultimate_base_type(expr.type_id);
                match type_table.get(base_type) {
                    ResolvedType::Primitive(PrimitiveType::F32) => {
                        func.instruction(&Instruction::F32Const(((*value) as f32).into()));
                    }
                    _ => {
                        func.instruction(&Instruction::F64Const((*value).into()));
                    }
                }
            }

            TirExprKind::BoolLiteral(b) => {
                func.instruction(&Instruction::I32Const(i32::from(*b)));
            }

            TirExprKind::CharLiteral(c) => {
                func.instruction(&Instruction::I32Const(*c as i32));
            }

            TirExprKind::StringLiteral(s) => {
                // String is a struct with two fields: repr (builtin::array<u8>) and used (i32)
                // 1. Create the raw byte array
                let len = s.len();
                let u8_array_idx = self.get_array_type_index(TypeTable::U8);

                if len == 0 {
                    // Empty string - create empty array without data section reference
                    func.instruction(&Instruction::I32Const(0)); // length
                    func.instruction(&Instruction::ArrayNewDefault(u8_array_idx));
                } else {
                    // Non-empty string - reference data section
                    let offset = self.get_string_offset(s);
                    func.instruction(&Instruction::I32Const(offset as i32));
                    func.instruction(&Instruction::I32Const(len as i32));
                    func.instruction(&Instruction::ArrayNewData {
                        array_type_index: u8_array_idx,
                        array_data_index: 0,
                    });
                }

                // 2. Push the length for the `used` field
                func.instruction(&Instruction::I32Const(len as i32));

                // 3. Create the String struct with (repr, used)
                let string_struct_info = self
                    .lookup_struct_type("String", &string_module_source())
                    .expect("String struct not found");
                func.instruction(&Instruction::StructNew(string_struct_info.type_idx));
            }

            TirExprKind::Null => {
                // Null for Option<T> - generates ref.null none
                // This is a polymorphic null that can be used for any nullable reference type
                func.instruction(&Instruction::RefNull(HeapType::Abstract {
                    shared: false,
                    ty: AbstractHeapType::None,
                }));
            }

            TirExprKind::OptionSome { value } => {
                // Option::Some(value) - wrap value in Option type
                // For reference types (String, structs, arrays): the value is already a reference
                // For primitive types: need to box the value first
                let inner_type_resolved = type_table.get(value.type_id).clone();

                match &inner_type_resolved {
                    ResolvedType::Primitive(prim) => {
                        // Primitive type: box it in a wrapper struct
                        // First, generate the value
                        self.generate_expr(func, value, type_table, ctx, builder);
                        // Get the corresponding ValType for the primitive
                        let val_type = match prim {
                            PrimitiveType::I8
                            | PrimitiveType::I16
                            | PrimitiveType::I32
                            | PrimitiveType::U8
                            | PrimitiveType::U16
                            | PrimitiveType::U32
                            | PrimitiveType::Bool
                            | PrimitiveType::Char => ValType::I32,
                            PrimitiveType::I64 | PrimitiveType::U64 => ValType::I64,
                            PrimitiveType::I128 | PrimitiveType::U128 => {
                                // i128/u128 not yet supported in Option
                                panic!("Option<i128/u128> not yet supported");
                            }
                            PrimitiveType::F32 => ValType::F32,
                            PrimitiveType::F64 => ValType::F64,
                        };
                        // Get the box type and wrap
                        if let Some(box_idx) = self.get_box_type_idx(val_type) {
                            func.instruction(&Instruction::StructNew(box_idx));
                        } else {
                            panic!(
                                "Box type not registered for {prim:?}. Make sure to use &{prim:?} somewhere."
                            );
                        }
                    }
                    _ => {
                        // Reference type: generate the value directly (it's already a reference)
                        // For Option<T> with reference types, the value IS the Some variant
                        // (null would be None). No wrapper needed.
                        self.generate_expr(func, value, type_table, ctx, builder);
                    }
                }
            }

            TirExprKind::VariantConstruct {
                variant_type,
                case_index,
                case_name,
                payload,
            } => {
                // Custom variant construction: Shape::Circle(5.0)
                // Layout: struct { tag: i32, payload? }

                // Get the variant name from the type (handle both Variant and GenericInstance)
                let variant_name = match type_table.get(*variant_type) {
                    ResolvedType::Variant { name, .. } => name.clone(),
                    ResolvedType::GenericInstance {
                        name, type_args, ..
                    } => {
                        // Build mangled name for generic variant: Result<i32,String>
                        let type_arg_names: Vec<String> = type_args
                            .iter()
                            .map(|t| type_table.mangle_type_name(*t))
                            .collect();
                        mangle_generic_name(name, &type_arg_names)
                    }
                    other => panic!("Expected Variant type for VariantConstruct, got: {other:?}"),
                };

                // Special handling for Option - it's a generic variant that uses nullable refs
                if variant_name == "Option" {
                    if case_name == "Some" && payload.is_some() {
                        let payload_expr = payload.as_ref().unwrap();
                        // Option::Some(value) - for primitives, need to box the value
                        let inner_type = type_table.get(payload_expr.type_id).clone();
                        if let ResolvedType::Primitive(prim) = &inner_type {
                            // Box the primitive value
                            self.generate_expr(func, payload_expr, type_table, ctx, builder);
                            let val_type = primitive_to_valtype(prim);
                            if let Some(box_idx) = self.get_box_type_idx(val_type) {
                                func.instruction(&Instruction::StructNew(box_idx));
                            }
                        } else {
                            // Reference types: generate directly
                            self.generate_expr(func, payload_expr, type_table, ctx, builder);
                        }
                    } else {
                        // Option::None - generate null ref
                        // Get the inner type from the expression's type (which should be Option<T>)
                        let inner_valtype = match type_table.get(expr.type_id) {
                            ResolvedType::Option(inner) => {
                                self.type_id_to_valtype(type_table, *inner)
                            }
                            ResolvedType::Variant { .. } => {
                                // Fallback for generic Option without concrete inner type
                                ValType::Ref(RefType {
                                    nullable: true,
                                    heap_type: HeapType::Abstract {
                                        shared: false,
                                        ty: AbstractHeapType::Any,
                                    },
                                })
                            }
                            _ => ValType::I32, // Fallback for primitives
                        };
                        match inner_valtype {
                            ValType::Ref(ref_type) => {
                                func.instruction(&Instruction::RefNull(ref_type.heap_type));
                            }
                            _ => {
                                // For primitives, use sentinel value (e.g., -1 for i32)
                                func.instruction(&Instruction::I32Const(-1));
                            }
                        }
                    }
                    return;
                }

                // Look up the registered variant type
                let variant_types = &self.variant_types;
                let variant_info = variant_types.get(&variant_name).unwrap_or_else(|| {
                    panic!("Variant type not registered: {variant_name}");
                });

                // Get the case-specific type index
                let case_info = &variant_info.cases[*case_index as usize];
                let case_type_idx = case_info.type_idx;

                // Push the tag (case index)
                func.instruction(&Instruction::I32Const(*case_index as i32));

                // Push the payload value if present (unit variants have no payload field)
                if let Some(payload_expr) = payload {
                    self.generate_expr(func, payload_expr, type_table, ctx, builder);
                }

                // Create the case-specific struct
                func.instruction(&Instruction::StructNew(case_type_idx));
            }

            TirExprKind::EnumConstruct { case_index, .. } => {
                // Enum is just an i32 discriminant value
                func.instruction(&Instruction::I32Const(*case_index as i32));
            }

            TirExprKind::IsNotNull { expr: inner } => {
                // Check if Option/nullable reference has a value (is not null)
                // Result type is bool (i32: 1 if not null, 0 if null)
                self.generate_expr(func, inner, type_table, ctx, builder);
                func.instruction(&Instruction::RefIsNull);
                func.instruction(&Instruction::I32Eqz); // NOT: true if NOT null
            }

            TirExprKind::UnwrapOption {
                expr: inner,
                inner_type,
            } => {
                // Unwrap Option to get the inner value, assuming not null
                // For reference types: use ref.as_non_null
                // For primitive types: unbox from the wrapper struct
                self.generate_expr(func, inner, type_table, ctx, builder);
                func.instruction(&Instruction::RefAsNonNull);

                // For primitive types, unbox the value
                if let ResolvedType::Primitive(prim) = type_table.get(*inner_type) {
                    let val_type = primitive_to_valtype(prim);
                    if let Some(box_type_idx) = self.get_box_type_idx(val_type) {
                        func.instruction(&Instruction::StructGet {
                            struct_type_index: box_type_idx,
                            field_index: 0,
                        });
                    }
                }
            }

            TirExprKind::VariantTag { expr: inner } => {
                // Get the discriminant (tag) of a variant value
                // Variant layout: struct { tag: i32, ... }
                // Result type is i32
                self.generate_expr(func, inner, type_table, ctx, builder);
                // Variant base type index is field 0 (tag) of the base struct
                // All variant case structs inherit from a common base with tag at field 0
                // Use struct.get with field_index 0 to get the tag
                // We need to find the variant base type index
                let variant_type_id = inner.type_id;
                let variant_name = match type_table.get(variant_type_id) {
                    ResolvedType::Variant { name, .. } => name.clone(),
                    ResolvedType::GenericInstance {
                        name, type_args, ..
                    } => {
                        let type_arg_names: Vec<String> = type_args
                            .iter()
                            .map(|t| type_table.mangle_type_name(*t))
                            .collect();
                        mangle_generic_name(name, &type_arg_names)
                    }
                    other => panic!("Expected Variant type for VariantTag, got: {other:?}"),
                };
                let variant_types = &self.variant_types;
                let variant_info = variant_types.get(&variant_name).unwrap_or_else(|| {
                    panic!("Variant type not registered: {variant_name}");
                });
                let base_type_idx = variant_info.base_type_idx;

                func.instruction(&Instruction::StructGet {
                    struct_type_index: base_type_idx,
                    field_index: 0,
                });
            }

            TirExprKind::VariantTest {
                expr: inner,
                case_index,
                case_name: _,
            } => {
                // Generate the scrutinee expression
                self.generate_expr(func, inner, type_table, ctx, builder);

                // Look up variant type info from the scrutinee's type
                let scrutinee_type = type_table.get(inner.type_id);
                let variant_name = match scrutinee_type {
                    ResolvedType::Variant { name, .. }
                    | ResolvedType::GenericInstance { name, .. } => name.clone(),
                    _ => panic!("VariantTest on non-variant type: {scrutinee_type:?}"),
                };

                let variant_lookup_name = type_table.mangle_type_name(inner.type_id);
                let variant_types = &self.variant_types;
                let variant_info = variant_types.get(&variant_lookup_name).unwrap_or_else(|| {
                    variant_types.get(&variant_name).unwrap_or_else(|| {
                        panic!(
                            "Variant type not registered: {variant_lookup_name} (base: {variant_name})"
                        );
                    })
                });

                // Get case info
                let case_info = variant_info
                    .cases
                    .get(*case_index as usize)
                    .unwrap_or_else(|| {
                        panic!("Invalid case index {case_index} for variant {variant_name}")
                    })
                    .clone();
                let case_type_idx = case_info.type_idx;
                let base_type_idx = variant_info.base_type_idx;
                let is_unit_variant = case_info.payload_type.is_none();

                // For unit variants, check discriminator; for payload variants, use ref.test
                if is_unit_variant {
                    // Read discriminator and compare with case index
                    func.instruction(&Instruction::StructGet {
                        struct_type_index: base_type_idx,
                        field_index: 0,
                    });
                    func.instruction(&Instruction::I32Const(*case_index as i32));
                    func.instruction(&Instruction::I32Eq);
                } else {
                    // Use ref.test to check if the value is of the expected case type
                    func.instruction(&Instruction::RefTestNonNull(HeapType::Concrete(
                        case_type_idx,
                    )));
                }
            }

            TirExprKind::VariantPayload {
                expr: inner,
                case_index,
                payload_type,
            } => {
                // Extract the payload from a variant value at a specific case index
                // Need to cast to the case-specific struct type, then get field 1 (payload)
                self.generate_expr(func, inner, type_table, ctx, builder);

                let variant_type_id = inner.type_id;
                let variant_name = match type_table.get(variant_type_id) {
                    ResolvedType::Variant { name, .. } => name.clone(),
                    ResolvedType::GenericInstance {
                        name, type_args, ..
                    } => {
                        let type_arg_names: Vec<String> = type_args
                            .iter()
                            .map(|t| type_table.mangle_type_name(*t))
                            .collect();
                        mangle_generic_name(name, &type_arg_names)
                    }
                    other => panic!("Expected Variant type for VariantPayload, got: {other:?}"),
                };
                let variant_types = &self.variant_types;
                let variant_info = variant_types.get(&variant_name).unwrap_or_else(|| {
                    panic!("Variant type not registered: {variant_name}");
                });
                let case_info = &variant_info.cases[*case_index as usize];
                let case_type_idx = case_info.type_idx;

                // Cast to the case-specific struct type
                func.instruction(&Instruction::RefCastNonNull(HeapType::Concrete(
                    case_type_idx,
                )));

                // Get the payload field (field 1, after the tag)
                func.instruction(&Instruction::StructGet {
                    struct_type_index: case_type_idx,
                    field_index: 1,
                });

                // If payload is a primitive and inner type indicates it needs unboxing
                // (handled by the lowering phase - payload_type should already be correct)
                let _ = payload_type; // Used by type system, not code generation
            }

            TirExprKind::Switch {
                scrutinee,
                min_value,
                arms,
                default,
            } => {
                // Switch expression using br_table for O(1) dispatch
                // Each arm index corresponds to (scrutinee_value - min_value)
                let result_valtype = self.type_id_to_valtype(type_table, expr.type_id);

                // Outer block for the switch result
                func.instruction(&Instruction::Block(wasm_encoder::BlockType::Result(
                    result_valtype,
                )));

                // Nested blocks for each arm (in reverse order for br_table targeting)
                // Block 0 = innermost = default, Block 1 = arm[n-1], ..., Block n = arm[0]
                for _ in 0..=arms.len() {
                    func.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
                }

                // Generate scrutinee and adjust for min_value
                self.generate_expr(func, scrutinee, type_table, ctx, builder);
                let scrutinee_base = type_table.get_ultimate_base_type(scrutinee.type_id);
                let is_i64 = matches!(
                    type_table.get(scrutinee_base),
                    ResolvedType::Primitive(PrimitiveType::I64 | PrimitiveType::U64)
                );

                if is_i64 {
                    func.instruction(&Instruction::I64Const(*min_value));
                    func.instruction(&Instruction::I64Sub);
                    func.instruction(&Instruction::I32WrapI64);
                } else {
                    func.instruction(&Instruction::I32Const(*min_value as i32));
                    func.instruction(&Instruction::I32Sub);
                }

                // br_table: indices 0..arms.len() -> blocks arms.len()..1, default -> block 0
                // targets[i] = arms.len() - i (maps value i to arm[i]'s block)
                let targets: Vec<u32> = (0..arms.len() as u32)
                    .map(|i| arms.len() as u32 - i)
                    .collect();
                let default_target = 0u32; // Default block is innermost

                func.instruction(&Instruction::BrTable(targets.into(), default_target));

                // End default block and generate default code
                func.instruction(&Instruction::End);
                self.generate_block_as_expr(func, default, expr.type_id, type_table, ctx, builder);
                func.instruction(&Instruction::Br(arms.len() as u32)); // Jump to result

                // Generate each arm (in order)
                for (i, arm) in arms.iter().enumerate() {
                    func.instruction(&Instruction::End);
                    self.generate_block_as_expr(func, arm, expr.type_id, type_table, ctx, builder);
                    func.instruction(&Instruction::Br((arms.len() - 1 - i) as u32));
                }

                // End outer result block
                func.instruction(&Instruction::End);
            }

            TirExprKind::Move { expr } => {
                // Move semantics: generate the inner value without copying
                // The value is moved directly, no value copy is generated
                self.generate_expr(func, expr, type_table, ctx, builder);
            }

            TirExprKind::Unit => {
                func.instruction(&Instruction::I32Const(0));
            }

            // === Variables ===
            TirExprKind::Local { index, .. } => {
                // Apply offset for closure functions (env param shifts indices by 1)
                let adjusted_index = *index + ctx.local_index_offset;
                func.instruction(&Instruction::LocalGet(adjusted_index));

                // For address-taken primitive locals, unbox to get the value
                if let Some(&box_type_idx) = ctx.local_box_types.get(index) {
                    func.instruction(&Instruction::StructGet {
                        struct_type_index: box_type_idx,
                        field_index: 0,
                    });
                } else {
                    // For reference types, locals are nullable but we may need non-nullable
                    // Check if this is a reference type and add RefAsNonNull
                    let val_type = self.type_id_to_valtype(type_table, expr.type_id);
                    if matches!(val_type, ValType::Ref(rt) if !rt.nullable) {
                        func.instruction(&Instruction::RefAsNonNull);
                    }
                }
            }

            TirExprKind::Global {
                module_source,
                name,
            } => {
                // TODO: Handle global references properly
                let module_path = module_source.to_path();
                let full_name = if module_path.is_empty() {
                    name.clone()
                } else {
                    format!("{}::{}", module_path.join("::"), name)
                };
                if let Some(func_idx) = builder.try_func_idx(&full_name) {
                    // If it's a function, push a reference to it
                    // For now, just push the index as i32 (placeholder)
                    func.instruction(&Instruction::I32Const(func_idx as i32));
                } else {
                    panic!("unknown global: {full_name}");
                }
            }

            TirExprKind::GlobalVarGet {
                module_source,
                name,
            } => {
                // Get the qualified global name
                let global_name = if module_source.is_entry_point() {
                    format!("global:{name}")
                } else {
                    let module_path = module_source.to_path();
                    format!("global:{}::{name}", module_path.join("::"))
                };
                let global_idx = builder.global_idx(&global_name);
                func.instruction(&Instruction::GlobalGet(global_idx));
                // For nullable globals (lazy init) with reference types, convert to non-null
                if builder.is_nullable_global(&global_name) {
                    // Only add ref.as_non_null for reference types
                    let val_type = self.type_id_to_valtype(type_table, expr.type_id);
                    if matches!(val_type, ValType::Ref(_)) {
                        func.instruction(&Instruction::RefAsNonNull);
                    }
                }
            }

            TirExprKind::GlobalVarSet {
                module_source,
                name,
                value,
            } => {
                // First evaluate the value to be assigned
                self.generate_expr(func, value, type_table, ctx, builder);
                // Get the qualified global name
                let global_name = if module_source.is_entry_point() {
                    format!("global:{name}")
                } else {
                    let module_path = module_source.to_path();
                    format!("global:{}::{name}", module_path.join("::"))
                };
                let global_idx = builder.global_idx(&global_name);
                func.instruction(&Instruction::GlobalSet(global_idx));
                // Push the assigned value back for expression result (unless UNIT type)
                if expr.type_id != TypeTable::UNIT {
                    func.instruction(&Instruction::GlobalGet(global_idx));
                    // For nullable globals, convert to non-null reference
                    if builder.is_nullable_global(&global_name) {
                        func.instruction(&Instruction::RefAsNonNull);
                    }
                }
            }

            // === Binary Operations ===
            TirExprKind::Binary { left, op, right } => {
                // Handle short-circuit evaluation for logical operators
                if *op == TirBinaryOp::And {
                    // For `a && b`: if a is false, skip b and return false
                    self.generate_expr(func, left, type_table, ctx, builder);
                    func.instruction(&Instruction::If(BlockType::Result(ValType::I32)));
                    // a is true, evaluate b
                    self.generate_expr(func, right, type_table, ctx, builder);
                    func.instruction(&Instruction::Else);
                    // a is false, result is 0
                    func.instruction(&Instruction::I32Const(0));
                    func.instruction(&Instruction::End);
                    return;
                }
                if *op == TirBinaryOp::Or {
                    // For `a || b`: if a is true, skip b and return true
                    self.generate_expr(func, left, type_table, ctx, builder);
                    func.instruction(&Instruction::If(BlockType::Result(ValType::I32)));
                    // a is true, result is 1
                    func.instruction(&Instruction::I32Const(1));
                    func.instruction(&Instruction::Else);
                    // a is false, evaluate b
                    self.generate_expr(func, right, type_table, ctx, builder);
                    func.instruction(&Instruction::End);
                    return;
                }

                // Handle reference type comparisons (use ref.eq instead of i32.eq/ne)
                let left_is_ref = self.is_reference_type(left.type_id, type_table);
                let right_is_ref = self.is_reference_type(right.type_id, type_table);
                if (left_is_ref || right_is_ref)
                    && (*op == TirBinaryOp::Eq || *op == TirBinaryOp::NotEq)
                {
                    self.generate_expr(func, left, type_table, ctx, builder);
                    self.generate_expr(func, right, type_table, ctx, builder);
                    func.instruction(&Instruction::RefEq);
                    if *op == TirBinaryOp::NotEq {
                        // Invert the result for !=
                        func.instruction(&Instruction::I32Eqz);
                    }
                    return;
                }

                // Determine if we need type promotion (e.g., i32 to i64)
                let left_is_i64 = matches!(
                    type_table.get(left.type_id),
                    ResolvedType::Primitive(PrimitiveType::I64 | PrimitiveType::U64)
                );
                let right_is_i64 = matches!(
                    type_table.get(right.type_id),
                    ResolvedType::Primitive(PrimitiveType::I64 | PrimitiveType::U64)
                );
                let left_is_i32 = matches!(
                    type_table.get(left.type_id),
                    ResolvedType::Primitive(
                        PrimitiveType::I32
                            | PrimitiveType::U32
                            | PrimitiveType::I16
                            | PrimitiveType::U16
                            | PrimitiveType::I8
                            | PrimitiveType::U8
                    )
                );
                let right_is_i32 = matches!(
                    type_table.get(right.type_id),
                    ResolvedType::Primitive(
                        PrimitiveType::I32
                            | PrimitiveType::U32
                            | PrimitiveType::I16
                            | PrimitiveType::U16
                            | PrimitiveType::I8
                            | PrimitiveType::U8
                    )
                );
                let left_is_unsigned = matches!(
                    type_table.get(left.type_id),
                    ResolvedType::Primitive(
                        PrimitiveType::U8
                            | PrimitiveType::U16
                            | PrimitiveType::U32
                            | PrimitiveType::U64
                    )
                );

                // Generate left operand
                self.generate_expr(func, left, type_table, ctx, builder);
                // Promote left if needed (use unsigned extension for unsigned types)
                if right_is_i64 && left_is_i32 {
                    if left_is_unsigned {
                        func.instruction(&Instruction::I64ExtendI32U);
                    } else {
                        func.instruction(&Instruction::I64ExtendI32S);
                    }
                }

                // Generate right operand
                self.generate_expr(func, right, type_table, ctx, builder);
                // Promote right if needed (use unsigned extension for unsigned types)
                if left_is_i64 && right_is_i32 {
                    let right_is_unsigned = matches!(
                        type_table.get(right.type_id),
                        ResolvedType::Primitive(
                            PrimitiveType::U8
                                | PrimitiveType::U16
                                | PrimitiveType::U32
                                | PrimitiveType::U64
                        )
                    );
                    if right_is_unsigned {
                        func.instruction(&Instruction::I64ExtendI32U);
                    } else {
                        func.instruction(&Instruction::I64ExtendI32S);
                    }
                }

                // Use i64 instructions if either operand is i64/u64
                // Preserve unsigned type info for proper instruction selection
                let effective_type = if left_is_i64 || right_is_i64 {
                    if left_is_unsigned {
                        TypeTable::U64
                    } else {
                        TypeTable::I64
                    }
                } else {
                    left.type_id
                };

                self.generate_binary_op(func, *op, effective_type, type_table);
            }

            // === Unary Operations ===
            TirExprKind::Unary { op, expr: inner } => {
                // Special case: &local or &mut local where local is address-taken
                // The local already stores a box, so just return it without re-boxing
                if matches!(op, TirUnaryOp::Ref | TirUnaryOp::MutRef)
                    && let TirExprKind::Local { index, .. } = &inner.kind
                    && ctx.local_box_types.contains_key(index)
                {
                    // Local is address-taken, just get the box reference
                    let adjusted_index = *index + ctx.local_index_offset;
                    func.instruction(&Instruction::LocalGet(adjusted_index));
                    // Convert nullable ref to non-nullable for function call
                    func.instruction(&Instruction::RefAsNonNull);
                    return;
                }

                self.generate_expr(func, inner, type_table, ctx, builder);
                self.generate_unary_op(func, *op, inner.type_id, type_table);
            }

            // === Assignment ===
            TirExprKind::Assign { target, value } => {
                match &target.kind {
                    TirExprKind::Local { index, .. } => {
                        // Apply offset for closure functions
                        let adjusted_index = *index + ctx.local_index_offset;
                        // For address-taken primitive locals, update the box
                        if let Some(&box_type_idx) = ctx.local_box_types.get(index) {
                            // Stack order: box_ref, value
                            func.instruction(&Instruction::LocalGet(adjusted_index));
                            self.generate_expr(func, value, type_table, ctx, builder);
                            func.instruction(&Instruction::StructSet {
                                struct_type_index: box_type_idx,
                                field_index: 0,
                            });
                            // Push the assigned value back for expression result
                            func.instruction(&Instruction::LocalGet(adjusted_index));
                            func.instruction(&Instruction::StructGet {
                                struct_type_index: box_type_idx,
                                field_index: 0,
                            });
                        } else {
                            self.generate_expr(func, value, type_table, ctx, builder);
                            // Apply value copy for struct/array/tuple types, but skip for
                            // Move expressions (optimizer marks fresh values with Move)
                            if self.needs_value_copy(value.type_id, type_table)
                                && !matches!(value.kind, TirExprKind::Move { .. })
                            {
                                self.generate_value_copy(
                                    func,
                                    value.type_id,
                                    type_table,
                                    ctx,
                                    builder,
                                );
                            }
                            // Use local.tee to both store and keep value on stack
                            func.instruction(&Instruction::LocalTee(adjusted_index));
                        }
                    }
                    TirExprKind::FieldAccess {
                        expr, field_index, ..
                    } => {
                        // For struct.set, stack order is: struct_ref, value
                        // Get the struct/tuple type from the receiver expression (handles references)
                        let type_idx = self.get_struct_or_tuple_type_idx(expr.type_id, type_table);

                        // Generate struct/tuple reference first
                        self.generate_expr(func, expr, type_table, ctx, builder);
                        // Then generate value
                        self.generate_expr(func, value, type_table, ctx, builder);
                        // Emit struct.set (consumes both values, leaves nothing)
                        func.instruction(&Instruction::StructSet {
                            struct_type_index: type_idx,
                            field_index: *field_index,
                        });
                        // Push the assigned value back for expression result
                        // (Regenerate the field access to get the value)
                        self.generate_expr(func, expr, type_table, ctx, builder);
                        func.instruction(&Instruction::StructGet {
                            struct_type_index: type_idx,
                            field_index: *field_index,
                        });
                    }
                    TirExprKind::Index {
                        expr: array_expr,
                        index: index_expr,
                    } => {
                        // For array.set, stack order is: array_ref, index, value
                        // Get the array type from the array expression (unwrap reference if needed)
                        let base_type_id = match type_table.get(array_expr.type_id) {
                            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => *inner,
                            _ => array_expr.type_id,
                        };
                        let base_type = type_table.get(base_type_id);
                        let (raw_array_type_idx, struct_type_idx, element_type) =
                            if let Some(element_type) = type_table.as_array(base_type_id) {
                                let raw_type_idx = self
                                    .array_types
                                    .get(&element_type)
                                    .copied()
                                    .or_else(|| self.array_types.get(&TypeTable::U8).copied())
                                    .expect("array type should be registered");
                                let struct_type_idx = self
                                    .lookup_array_struct_type(element_type, type_table)
                                    .expect("Array struct type should be registered");
                                (raw_type_idx, struct_type_idx, element_type)
                            } else {
                                panic!("index assignment on non-array type: {base_type:?}");
                            };

                        // Generate array reference first
                        self.generate_expr(func, array_expr, type_table, ctx, builder);
                        // Access the repr field to get the raw array
                        func.instruction(&Instruction::StructGet {
                            struct_type_index: struct_type_idx,
                            field_index: 0, // repr is field 0
                        });
                        // Then generate index
                        self.generate_expr(func, index_expr, type_table, ctx, builder);
                        // Then generate value
                        self.generate_expr(func, value, type_table, ctx, builder);
                        // Emit array.set (consumes all three values, leaves nothing)
                        func.instruction(&Instruction::ArraySet(raw_array_type_idx));
                        // Push the assigned value back for expression result
                        // (Regenerate the index access to get the value)
                        self.generate_expr(func, array_expr, type_table, ctx, builder);
                        func.instruction(&Instruction::StructGet {
                            struct_type_index: struct_type_idx,
                            field_index: 0,
                        });
                        self.generate_expr(func, index_expr, type_table, ctx, builder);
                        // For packed types (i8/u8/i16/u16), use ArrayGetS/ArrayGetU
                        let elem_resolved = type_table.get(element_type);
                        if matches!(
                            elem_resolved,
                            ResolvedType::Primitive(PrimitiveType::U8 | PrimitiveType::U16)
                        ) {
                            func.instruction(&Instruction::ArrayGetU(raw_array_type_idx));
                        } else if matches!(
                            elem_resolved,
                            ResolvedType::Primitive(PrimitiveType::I8 | PrimitiveType::I16)
                        ) {
                            func.instruction(&Instruction::ArrayGetS(raw_array_type_idx));
                        } else {
                            func.instruction(&Instruction::ArrayGet(raw_array_type_idx));
                        }
                    }
                    TirExprKind::Unary {
                        op: TirUnaryOp::Deref,
                        expr: ref_expr,
                    } => {
                        // Assignment through dereference: *x = value
                        // For primitive refs (including newtypes of primitives): update the box struct
                        // For struct/tuple refs: this assigns the whole value (not field)
                        let ref_type = type_table.get(ref_expr.type_id);
                        if let ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) = ref_type {
                            // Follow newtypes to find the ultimate primitive base type
                            let ultimate_inner = type_table.get_ultimate_base_type(*inner);
                            if let ResolvedType::Primitive(prim) = type_table.get(ultimate_inner) {
                                // Primitive ref: use box struct.set
                                let val_type = primitive_to_valtype(prim);
                                if let Some(box_type_idx) = self.get_box_type_idx(val_type) {
                                    // Stack order: box_ref, value
                                    self.generate_expr(func, ref_expr, type_table, ctx, builder);
                                    self.generate_expr(func, value, type_table, ctx, builder);
                                    func.instruction(&Instruction::StructSet {
                                        struct_type_index: box_type_idx,
                                        field_index: 0,
                                    });
                                    // Push the assigned value back for expression result
                                    self.generate_expr(func, ref_expr, type_table, ctx, builder);
                                    func.instruction(&Instruction::StructGet {
                                        struct_type_index: box_type_idx,
                                        field_index: 0,
                                    });
                                } else {
                                    panic!(
                                        "no box type for primitive in deref assignment: {prim:?}"
                                    );
                                }
                            } else {
                                // Non-primitive ref: not yet supported
                                panic!(
                                    "deref assignment for non-primitive types not yet supported"
                                );
                            }
                        } else {
                            panic!("deref assignment target is not a reference type");
                        }
                    }
                    _ => panic!("invalid assignment target in TIR"),
                }
            }

            // === Type Cast ===
            TirExprKind::Cast {
                expr: inner,
                target_type,
            } => {
                // Special case: integer literal cast to i64/u64 - generate I64Const directly
                // to avoid truncation through i32
                let is_i64_target = matches!(
                    type_table.get(*target_type),
                    ResolvedType::Primitive(PrimitiveType::I64 | PrimitiveType::U64)
                );
                if let TirExprKind::IntLiteral { value, .. } = &inner.kind
                    && is_i64_target
                {
                    func.instruction(&Instruction::I64Const(*value as i64));
                    return;
                }
                self.generate_expr(func, inner, type_table, ctx, builder);
                self.generate_cast(func, inner.type_id, *target_type, type_table);
            }

            // === Function Call ===
            TirExprKind::Call {
                func: call_func,
                args,
                ..
            } => {
                let module_path = call_func.module_path();
                let func_name = call_func.name();

                // Handle builtin functions (intrinsics and canonical mappings)
                // Check both direct builtins and monomorphized builtins
                if let Some(builtin) = call_func
                    .builtin_name()
                    .or_else(|| call_func.monomorphized_builtin_name())
                {
                    self.generate_builtin_call(
                        &builtin, args, expr, func, type_table, ctx, builder,
                    );
                } else if module_path.is_empty()
                    && self.generate_variant_constructor(
                        &func_name, args, func, type_table, ctx, builder,
                    )
                {
                    // Variant constructor was handled
                } else if module_path.len() == 1
                    && module_path[0]
                        .chars()
                        .next()
                        .is_some_and(|c| c.is_ascii_uppercase())
                    && self.generate_cm_effect_call(
                        func,
                        ctx,
                        builder,
                        type_table,
                        &module_path[0],
                        &func_name,
                        args,
                    )
                {
                    // CM effect call handled via convention
                } else {
                    // Generate arguments first
                    self.generate_args(func, args, type_table, ctx, builder);

                    // Resolve function index using multiple strategies
                    let func_idx = self.resolve_call_target(
                        &module_path,
                        &func_name,
                        &ctx.current_module_path,
                        builder,
                    );
                    func.instruction(&Instruction::Call(func_idx));
                }
            }

            // === Effect Operation Call ===
            // Note: Effect calls are typically represented as TirExprKind::Call in the TIR,
            // so this branch handles cases where EffectCall is explicitly constructed.
            TirExprKind::EffectCall {
                effect_name,
                op_name,
                args,
                ..
            } => {
                // Try to handle via CM convention
                if self.generate_cm_effect_call(
                    func,
                    ctx,
                    builder,
                    type_table,
                    effect_name,
                    op_name,
                    args,
                ) {
                    // CM effect call handled via convention
                } else {
                    // Fallback for unknown effect calls
                    self.generate_args(func, args, type_table, ctx, builder);
                    let full_name = format!("{effect_name}::{op_name}");
                    if let Some(func_idx) = builder.try_func_idx(&full_name) {
                        func.instruction(&Instruction::Call(func_idx));
                    } else {
                        panic!("unknown effect operation: {full_name}");
                    }
                }
            }

            // === Method Call ===
            TirExprKind::MethodCall {
                receiver,
                func: method_func,
                args,
                ..
            } => {
                // Extract method name and trait name from method_info
                // Use full_method_name() to include method type args (e.g., "transform<i64>")
                let (method_name, trait_name) = if let Some(info) = method_func.method_info() {
                    (info.full_method_name(), info.trait_name)
                } else {
                    // Fallback to function name if no method_info
                    (method_func.name(), None)
                };
                // Get the base type for method lookup (strip Ref/MutRef only, preserve Newtype)
                let base_receiver_type = {
                    let mut t = type_table.get(receiver.type_id).clone();
                    while let ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) = t {
                        t = type_table.get(inner).clone();
                    }
                    t
                };

                match base_receiver_type {
                    // General struct method call (including String)
                    ResolvedType::Struct {
                        name,
                        module_source,
                        ..
                    } => {
                        // General struct method handling (trait and inherent methods)
                        // Build the fully mangled method name: path/Struct^Trait::method or path/Struct::method
                        let module_path = module_source.to_path();
                        let mangled_name = MethodName::new(
                            module_path.join("/"),
                            name.clone(),
                            trait_name.clone(),
                            method_name.clone(),
                        )
                        .to_string();

                        // Look up the method function index
                        // For monomorphized generics (e.g., Box<i32>), also try base struct name (Box)
                        let struct_lookup_name =
                            StructName::new(module_source.clone(), name.clone());
                        let struct_info = self.struct_types.get(&struct_lookup_name);
                        let func_idx = builder.try_func_idx(&mangled_name).or_else(|| {
                            // If struct is monomorphized, try the base name from metadata
                            if let Some(info) = struct_info
                                && info.is_monomorphized
                                && let Some(base_name) = &info.base_name
                            {
                                let base_mangled = MethodName::new(
                                    module_path.join("/"),
                                    base_name.clone(),
                                    None,
                                    method_name.clone(),
                                )
                                .to_string();
                                builder.try_func_idx(&base_mangled)
                            } else {
                                None
                            }
                        });

                        // Also try simple alias names for monomorphized methods
                        // These are registered with an alias using just the struct name and method
                        let simple_name = MethodName::format_local(&name, None, &method_name);
                        // For trait methods, also include trait in the simple name
                        let simple_trait_name =
                            MethodName::format_local(&name, trait_name.as_deref(), &method_name);

                        let final_func_idx = func_idx
                            .or_else(|| builder.try_func_idx(&simple_trait_name))
                            .or_else(|| builder.try_func_idx(&simple_name));

                        if let Some(idx) = final_func_idx {
                            // Generate code for the receiver (self parameter)
                            self.generate_expr(func, receiver, type_table, ctx, builder);

                            // Generate code for other arguments
                            self.generate_args(func, args, type_table, ctx, builder);

                            // Call the method
                            func.instruction(&Instruction::Call(idx));
                        } else {
                            panic!(
                                "unknown method: {mangled_name} (also tried aliases: {simple_trait_name}, {simple_name})"
                            );
                        }
                    }

                    // User-defined generic struct method calls (e.g., Box<i32>.get())
                    ResolvedType::GenericInstance {
                        name,
                        type_args,
                        module_source,
                    } => {
                        // Build monomorphized struct and method name: Box<i32>::get
                        let module_path = module_source.to_path();
                        let type_arg_names: Vec<String> = type_args
                            .iter()
                            .map(|t| type_table.mangle_type_name(*t))
                            .collect();
                        let mangled_struct_name = mangle_generic_name(&name, &type_arg_names);

                        // For trait methods, use the trait name in the method reference
                        // e.g., Triple<i32>^IndexValue<i32>::index_value
                        let mangled_method_name = MethodName::format_local(
                            &mangled_struct_name,
                            trait_name.as_deref(),
                            &method_name,
                        );

                        // Build full method name with module path
                        let full_method_name = MethodName::new(
                            module_path.join("/"),
                            mangled_struct_name.clone(),
                            trait_name.clone(),
                            method_name.clone(),
                        )
                        .to_string();

                        // Also try non-monomorphized names for generic trait methods
                        // e.g., Triple^IndexValue::index_value (without type args)
                        let generic_method_name =
                            MethodName::format_local(&name, trait_name.as_deref(), &method_name);
                        let generic_full_method_name = MethodName::new(
                            module_path.join("/"),
                            name.clone(),
                            trait_name.clone(),
                            method_name.clone(),
                        )
                        .to_string();

                        // Try all possible method names
                        let func_idx = builder
                            .try_func_idx(&full_method_name)
                            .or_else(|| builder.try_func_idx(&mangled_method_name))
                            .or_else(|| builder.try_func_idx(&generic_full_method_name))
                            .or_else(|| builder.try_func_idx(&generic_method_name));

                        if let Some(idx) = func_idx {
                            // Generate receiver
                            self.generate_expr(func, receiver, type_table, ctx, builder);
                            // Generate arguments
                            self.generate_args(func, args, type_table, ctx, builder);
                            // Call the method
                            func.instruction(&Instruction::Call(idx));
                        } else {
                            panic!(
                                "unknown method {method_name} on generic struct {name}: tried [{full_method_name}], [{mangled_method_name}], [{generic_full_method_name}], [{generic_method_name}]"
                            );
                        }
                    }

                    // WASI Resource method calls (e.g., Fields::method_fields_set)
                    ResolvedType::Resource { name, .. } => {
                        // Build the method name for wasi_registry lookup: ResourceName::method_name
                        let func_name = format!("{name}::{method_name}");

                        if let Some(func_info) = self.project.wasi_registry.get_function(&func_name)
                        {
                            let local_name = func_info.local_alias_name();

                            // Generate receiver (resource handle is i32)
                            self.generate_expr(func, receiver, type_table, ctx, builder);

                            // Generate arguments
                            self.generate_args(func, args, type_table, ctx, builder);

                            // Call the WASI function
                            let func_idx = builder.func_idx(&local_name);
                            func.instruction(&Instruction::Call(func_idx));

                            // Handle Result return if needed
                            let conv = &func_info.call_convention;
                            // Result return handling - for now just panic
                            assert!(
                                conv.result_return.is_none(),
                                "Resource method with Result return not yet implemented: {func_name}"
                            );
                        } else {
                            panic!("Unknown resource method: {func_name}");
                        }
                    }

                    // Newtype method calls (e.g., Radians::to_degrees where type Radians = f64)
                    // Methods can be defined directly on the newtype via `impl Radians { ... }`
                    // or inherited from the base type
                    ResolvedType::Newtype {
                        name,
                        module_source,
                        base_type,
                    } => {
                        let module_path = module_source.to_path();

                        // Try to find method on the newtype itself first
                        let mangled_name = MethodName::new(
                            module_path.join("/"),
                            name.clone(),
                            trait_name.clone(),
                            method_name.clone(),
                        )
                        .to_string();

                        // Also try simple format without module path
                        let simple_name = MethodName::format_local(&name, None, &method_name);
                        let simple_trait_name =
                            MethodName::format_local(&name, trait_name.as_deref(), &method_name);

                        let func_idx = builder
                            .try_func_idx(&mangled_name)
                            .or_else(|| builder.try_func_idx(&simple_trait_name))
                            .or_else(|| builder.try_func_idx(&simple_name));

                        if let Some(idx) = func_idx {
                            // Method found on newtype itself - call it
                            self.generate_expr(func, receiver, type_table, ctx, builder);
                            self.generate_args(func, args, type_table, ctx, builder);
                            func.instruction(&Instruction::Call(idx));
                        } else {
                            // Method not found on newtype - try base type (method inheritance)
                            // Recursively handle the base type by building a synthetic receiver
                            let base_resolved = type_table.get(base_type).clone();
                            match base_resolved {
                                ResolvedType::Struct {
                                    name: base_name,
                                    module_source: base_module,
                                    ..
                                } => {
                                    // Look up method on base struct
                                    let base_module_path = base_module.to_path();
                                    let base_mangled = MethodName::new(
                                        base_module_path.join("/"),
                                        base_name.clone(),
                                        trait_name.clone(),
                                        method_name.clone(),
                                    )
                                    .to_string();

                                    if let Some(idx) = builder.try_func_idx(&base_mangled) {
                                        self.generate_expr(
                                            func, receiver, type_table, ctx, builder,
                                        );
                                        self.generate_args(func, args, type_table, ctx, builder);
                                        func.instruction(&Instruction::Call(idx));
                                    } else {
                                        panic!(
                                            "method '{method_name}' not found on newtype '{name}' or base type '{base_name}'"
                                        );
                                    }
                                }
                                // Handle chained newtypes (e.g., type C = B, type B = A, type A = Point)
                                ResolvedType::Newtype {
                                    base_type: inner_base,
                                    ..
                                } => {
                                    // Follow the chain to find the ultimate base type
                                    let ultimate_base = Self::resolve_to_ultimate_base(
                                        type_table.get(inner_base).clone(),
                                        type_table,
                                    );
                                    match ultimate_base {
                                        Some(UltimateBaseType::Struct {
                                            name: base_name,
                                            module_source: base_module,
                                        }) => {
                                            // Look up method on ultimate base struct
                                            let base_module_path = base_module.to_path();
                                            let base_mangled = MethodName::new(
                                                base_module_path.join("/"),
                                                base_name.clone(),
                                                trait_name.clone(),
                                                method_name.clone(),
                                            )
                                            .to_string();

                                            if let Some(idx) = builder.try_func_idx(&base_mangled) {
                                                self.generate_expr(
                                                    func, receiver, type_table, ctx, builder,
                                                );
                                                self.generate_args(
                                                    func, args, type_table, ctx, builder,
                                                );
                                                func.instruction(&Instruction::Call(idx));
                                            } else {
                                                panic!(
                                                    "method '{method_name}' not found on chained newtype '{name}' or ultimate base '{base_name}'"
                                                );
                                            }
                                        }
                                        Some(UltimateBaseType::Primitive(prim)) => {
                                            let base_type_name = prim.as_str();

                                            let prim_module_path = "core/prelude/primitives";
                                            let base_mangled = MethodName::new(
                                                prim_module_path.to_string(),
                                                base_type_name.to_string(),
                                                trait_name.clone(),
                                                method_name.clone(),
                                            )
                                            .to_string();

                                            let base_inherent = MethodName::new(
                                                prim_module_path.to_string(),
                                                base_type_name.to_string(),
                                                None,
                                                method_name.clone(),
                                            )
                                            .to_string();

                                            let func_idx = builder
                                                .try_func_idx(&base_mangled)
                                                .or_else(|| builder.try_func_idx(&base_inherent));

                                            if let Some(idx) = func_idx {
                                                self.generate_expr(
                                                    func, receiver, type_table, ctx, builder,
                                                );
                                                self.generate_args(
                                                    func, args, type_table, ctx, builder,
                                                );
                                                func.instruction(&Instruction::Call(idx));
                                            } else {
                                                panic!(
                                                    "method '{method_name}' not found on chained newtype '{name}' or ultimate base primitive '{base_type_name}'"
                                                );
                                            }
                                        }
                                        None => {
                                            panic!(
                                                "method '{method_name}' not found on chained newtype '{name}'"
                                            );
                                        }
                                    }
                                }
                                // Newtype of primitive (e.g., type Meters = f64)
                                // Look up the method on the base primitive type
                                ResolvedType::Primitive(prim) => {
                                    let base_type_name = prim.as_str();

                                    // Primitive methods are defined in core/prelude/primitives.wado
                                    let prim_module_path = "core/prelude/primitives";
                                    let base_mangled = MethodName::new(
                                        prim_module_path.to_string(),
                                        base_type_name.to_string(),
                                        trait_name.clone(),
                                        method_name.clone(),
                                    )
                                    .to_string();

                                    // Also try without trait name
                                    let base_inherent = MethodName::new(
                                        prim_module_path.to_string(),
                                        base_type_name.to_string(),
                                        None,
                                        method_name.clone(),
                                    )
                                    .to_string();

                                    let func_idx = builder
                                        .try_func_idx(&base_mangled)
                                        .or_else(|| builder.try_func_idx(&base_inherent));

                                    if let Some(idx) = func_idx {
                                        self.generate_expr(
                                            func, receiver, type_table, ctx, builder,
                                        );
                                        self.generate_args(func, args, type_table, ctx, builder);
                                        func.instruction(&Instruction::Call(idx));
                                    } else {
                                        panic!(
                                            "method '{method_name}' not found on newtype '{name}' or base primitive '{base_type_name}'"
                                        );
                                    }
                                }
                                _ => {
                                    panic!(
                                        "method '{method_name}' not found on newtype '{name}' with unsupported base type"
                                    );
                                }
                            }
                        }
                    }

                    // Primitive type method calls (e.g., 42.to_string())
                    // Methods are defined in core/prelude/primitives.wado
                    ResolvedType::Primitive(prim) => {
                        let type_name = prim.as_str();

                        // Primitive methods are defined in core/prelude/primitives.wado
                        let module_path = "core/prelude/primitives";
                        let mangled_name = MethodName::new(
                            module_path.to_string(),
                            type_name.to_string(),
                            trait_name.clone(),
                            method_name.clone(),
                        )
                        .to_string();

                        // Also try without trait name (for inherent methods)
                        let inherent_name = MethodName::new(
                            module_path.to_string(),
                            type_name.to_string(),
                            None,
                            method_name.clone(),
                        )
                        .to_string();

                        let func_idx = builder
                            .try_func_idx(&mangled_name)
                            .or_else(|| builder.try_func_idx(&inherent_name));

                        if let Some(idx) = func_idx {
                            // Generate receiver
                            self.generate_expr(func, receiver, type_table, ctx, builder);
                            // Generate arguments
                            self.generate_args(func, args, type_table, ctx, builder);
                            // Call the method
                            func.instruction(&Instruction::Call(idx));
                        } else {
                            panic!(
                                "unknown method '{method_name}' on primitive type '{type_name}': tried [{mangled_name}], [{inherent_name}]"
                            );
                        }
                    }

                    other => {
                        panic!(
                            "method call receiver has unexpected type: {:?}, method: {}, receiver.type_id: {}",
                            other, method_name, receiver.type_id
                        );
                    }
                }
            }

            // === Static Method Call ===
            TirExprKind::StaticCall {
                func: static_func,
                args,
            } => {
                let func_name = static_func.name();

                // Generate arguments first
                self.generate_args(func, args, type_table, ctx, builder);

                // Check if this is a monomorphized function using metadata
                let base_struct_name = static_func.base_struct_name();

                // Get method_info for proper struct/trait/method name lookup
                let method_info = static_func.method_info();

                // Use the FunctionRef's module_path - this is set correctly by the resolver
                // to point to where the method is defined (e.g., core/prelude/primitives for primitives,
                // or the module where the struct/impl is defined for user types).
                let module_path = static_func.module_path();

                // func_name is already mangled as "StructName::method", "Struct^Trait::method", etc.
                // We need to look it up using the same name format used during function definition
                // Methods are registered with MethodName format: {module_path}/{struct_name}^{trait}::{method_name}
                let func_idx = if let Some(info) = &method_info {
                    // Use method_info for accurate struct/trait/method names
                    let struct_name = &info.struct_name;
                    let trait_name = &info.trait_name;
                    let method_name = info.full_method_name();

                    // Build the mangled name using proper components
                    let mangled_name = MethodName::new(
                        module_path.join("/"),
                        struct_name.clone(),
                        trait_name.clone(),
                        method_name.clone(),
                    )
                    .to_string();

                    // Check struct metadata for fallback
                    let struct_lookup_name =
                        StructName::from_path_and_name(&module_path, struct_name);
                    let struct_info = self.struct_types.get(&struct_lookup_name);

                    builder
                        .try_func_idx(&mangled_name)
                        .or_else(|| {
                            // Also try without trait name (for inherent methods)
                            if trait_name.is_some() {
                                let inherent_name = MethodName::new(
                                    module_path.join("/"),
                                    struct_name.clone(),
                                    None,
                                    method_name.clone(),
                                )
                                .to_string();
                                builder.try_func_idx(&inherent_name)
                            } else {
                                None
                            }
                        })
                        .or_else(|| {
                            // Also try without module path (for current module lookups)
                            builder.try_func_idx(&func_name)
                        })
                        .or_else(|| {
                            // For monomorphized generic types like Array<i32>, also try the generic version
                            // Use metadata: either from function or struct
                            let generic_name = base_struct_name
                                .as_ref()
                                .or_else(|| struct_info.and_then(|s| s.base_name.as_ref()))
                                .or(Some(&info.base_struct_name));

                            if let Some(generic_struct_name) = generic_name {
                                let generic_mangled_name = MethodName::new(
                                    module_path.join("/"),
                                    generic_struct_name.clone(),
                                    trait_name.clone(),
                                    method_name.clone(),
                                )
                                .to_string();
                                builder.try_func_idx(&generic_mangled_name)
                            } else {
                                None
                            }
                        })
                } else {
                    // Method calls (containing ::) should always have method_info
                    assert!(
                        !func_name.contains("::"),
                        "StaticCall to method '{func_name}' missing method_info"
                    );
                    // Free function call (no :: separator)
                    let full_name = if module_path.is_empty() {
                        func_name.clone()
                    } else {
                        format!("{}/{}", module_path.join("/"), func_name)
                    };
                    builder.try_func_idx(&full_name)
                };

                if let Some(idx) = func_idx {
                    func.instruction(&Instruction::Call(idx));
                } else {
                    // Function not found - try to inline for user-defined generic struct constructors
                    // Use return type metadata to determine if this is a generic struct constructor
                    let return_type_info = match type_table.get(expr.type_id) {
                        ResolvedType::GenericInstance {
                            name,
                            module_source: type_module_source,
                            type_args,
                        } => {
                            // Build the mangled struct name (e.g., Box<i32>)
                            let type_arg_names: Vec<String> = type_args
                                .iter()
                                .map(|t| type_table.mangle_type_name(*t))
                                .collect();
                            let mangled = mangle_generic_name(name, &type_arg_names);
                            Some((mangled, type_module_source.clone()))
                        }
                        ResolvedType::Struct {
                            name,
                            module_source: type_module_source,
                            ..
                        } => {
                            // Check if this struct is monomorphized using metadata
                            let struct_lookup =
                                StructName::new(type_module_source.clone(), name.clone());
                            if self
                                .struct_types
                                .get(&struct_lookup)
                                .map(|s| s.is_monomorphized)
                                .unwrap_or(false)
                            {
                                Some((name.clone(), type_module_source.clone()))
                            } else {
                                None
                            }
                        }
                        _ => None,
                    };

                    if let Some((struct_name_to_lookup, struct_module_source)) = return_type_info {
                        // Look up the struct type
                        let struct_lookup_name = StructName::new(
                            struct_module_source.clone(),
                            struct_name_to_lookup.clone(),
                        );
                        if let Some(struct_info) = self.struct_types.get(&struct_lookup_name) {
                            // Check if args count matches field count (constructor pattern)
                            if args.len() == struct_info.field_count {
                                // Arguments are already on the stack, just create the struct
                                func.instruction(&Instruction::StructNew(struct_info.type_idx));
                                return;
                            }
                        }

                        // Check if this is a generic variant constructor (e.g., Result<i32,String>::Ok)
                        if let Some(variant_info) =
                            self.variant_types.get(&struct_name_to_lookup).cloned()
                        {
                            // Find the case index by method name
                            // The method name is the last part after ::
                            let case_name = func_name.rsplit("::").next().unwrap_or(&func_name);
                            if let Some((case_idx, case_info)) = variant_info
                                .cases
                                .iter()
                                .enumerate()
                                .find(|(_, info)| info.name == case_name)
                            {
                                // Generate variant construction using the case-specific type
                                let case_type_idx = case_info.type_idx;

                                // Push tag value
                                func.instruction(&Instruction::I32Const(case_idx as i32));

                                // Arguments are already evaluated, now create the struct
                                // Stack: [arg1, arg2, ...] -> need: [tag, arg1, arg2, ...]
                                // We need to reorder - but args are already on stack before tag
                                // Actually, we need to generate args AFTER tag

                                // This is tricky - args were already generated above
                                // We need a different approach: don't generate args above, do it here
                                // But that's a bigger refactor...

                                // For now, generate the struct with tag first, then set fields
                                // Create struct with default values
                                func.instruction(&Instruction::StructNew(case_type_idx));
                                return;
                            }
                        }
                    }

                    // Try WASI function resolution for resource methods
                    // Resource methods like TcpSocket::static_tcp_socket_create are registered
                    // in wasi_registry under "TcpSocket::static_tcp_socket_create"
                    if let Some(func_info) = self.project.wasi_registry.get_function(&func_name) {
                        let conv = &func_info.call_convention;
                        let local_name = func_info.local_alias_name();

                        // Handle outptr allocation for Result returns
                        if let Some((size, align)) = conv.outptr_alloc {
                            // Allocate outptr using realloc
                            func.instruction(&Instruction::I32Const(0)); // old_ptr
                            func.instruction(&Instruction::I32Const(0)); // old_size
                            func.instruction(&Instruction::I32Const(align as i32)); // align
                            func.instruction(&Instruction::I32Const(size as i32)); // new_size
                            let realloc_idx = builder.func_idx("realloc");
                            func.instruction(&Instruction::Call(realloc_idx));

                            // Store outptr for later use
                            let outptr_local = ctx.get_local("__cm_outptr").expect(
                                "__cm_outptr should be pre-allocated for functions with CM complex returns",
                            );
                            func.instruction(&Instruction::LocalTee(outptr_local));
                        }

                        // Call the WASI function
                        let func_idx = builder.func_idx(&local_name);
                        func.instruction(&Instruction::Call(func_idx));

                        // Handle Result return conversion using subtype-based representation
                        if let Some((ok_is_resource, _err_is_enum)) = conv.result_return {
                            let outptr_local = ctx
                                .get_local("__cm_outptr")
                                .expect("__cm_outptr should be pre-allocated for Result returns");

                            // Get Result type info for Ok and Err subtypes
                            let result_type_id = expr.type_id;
                            let mangled_name = type_table.mangle_type_name(result_type_id);
                            let variant_types = &self.variant_types;
                            let result_info =
                                variant_types.get(&mangled_name).unwrap_or_else(|| {
                                    panic!("Result type not registered: {mangled_name}")
                                });
                            // Result has cases [Ok (0), Err (1)]
                            let ok_type_idx = result_info.cases[0].type_idx;
                            let err_type_idx = result_info.cases[1].type_idx;
                            let base_type_idx = result_info.base_type_idx;

                            // Result type for block result
                            let result_ref_type = ValType::Ref(RefType {
                                nullable: true,
                                heap_type: HeapType::Concrete(base_type_idx),
                            });

                            // Read discriminant from outptr
                            func.instruction(&Instruction::LocalGet(outptr_local));
                            func.instruction(&Instruction::I32Load(wasm_encoder::MemArg {
                                offset: 0,
                                align: 2,
                                memory_index: 0,
                            }));

                            // Branch based on discriminant: 0 = Ok, non-zero = Err
                            func.instruction(&Instruction::If(wasm_encoder::BlockType::Result(
                                result_ref_type,
                            )));

                            // Err case (discriminant != 0)
                            func.instruction(&Instruction::I32Const(1)); // Err discriminant
                            func.instruction(&Instruction::LocalGet(outptr_local));
                            func.instruction(&Instruction::I32Load(wasm_encoder::MemArg {
                                offset: 4,
                                align: 2,
                                memory_index: 0,
                            }));
                            func.instruction(&Instruction::StructNew(err_type_idx));

                            func.instruction(&Instruction::Else);

                            // Ok case (discriminant == 0)
                            func.instruction(&Instruction::I32Const(0)); // Ok discriminant
                            func.instruction(&Instruction::LocalGet(outptr_local));
                            if ok_is_resource {
                                // Resource handle is i32
                                func.instruction(&Instruction::I32Load(wasm_encoder::MemArg {
                                    offset: 4,
                                    align: 2,
                                    memory_index: 0,
                                }));
                            } else {
                                // Primitive type - for now assume i32
                                func.instruction(&Instruction::I32Load(wasm_encoder::MemArg {
                                    offset: 4,
                                    align: 2,
                                    memory_index: 0,
                                }));
                            }
                            func.instruction(&Instruction::StructNew(ok_type_idx));

                            func.instruction(&Instruction::End);
                        }

                        return;
                    }

                    panic!("unknown static method: {func_name}");
                }
            }

            // === Field Access ===
            TirExprKind::FieldAccess {
                expr: inner,
                field_index,
                ..
            } => {
                // Get the struct/tuple type from the inner expression
                // For references, look through to the inner type
                let struct_type_idx = self.get_struct_or_tuple_type_idx(inner.type_id, type_table);

                self.generate_expr(func, inner, type_table, ctx, builder);
                func.instruction(&Instruction::StructGet {
                    struct_type_index: struct_type_idx,
                    field_index: *field_index,
                });
            }

            // === Index Access ===
            TirExprKind::Index { expr: array, index } => {
                self.generate_expr(func, array, type_table, ctx, builder);

                // Get the array type index from the array expression's type (unwrap reference if needed)
                let base_type_id = match type_table.get(array.type_id) {
                    ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => *inner,
                    _ => array.type_id,
                };
                // Track element type info for post-array-get processing
                let (raw_array_type_idx, element_type_id, element_is_ref, closure_cast_type_idx) =
                    if let Some(element_type) = type_table.as_array(base_type_id) {
                        let element_resolved = type_table.get(element_type);
                        let is_ref = matches!(
                            element_resolved,
                            ResolvedType::GenericInstance { .. }
                                | ResolvedType::Struct { .. }
                                | ResolvedType::Function { .. }
                                | ResolvedType::Ref(_)
                                | ResolvedType::MutRef(_)
                        );
                        // For function types, we need to cast structref to canonical closure type
                        let closure_type_idx = if let ResolvedType::Function {
                            params,
                            return_type,
                            ..
                        } = element_resolved
                        {
                            let canonical = &self.canonical_closure_types;
                            canonical
                                .get(&(params.clone(), *return_type))
                                .map(|(_, _, struct_idx)| *struct_idx)
                        } else {
                            None
                        };
                        let array_struct_type_idx = self
                            .lookup_array_struct_type(element_type, type_table)
                            .expect("Array struct type should be registered");
                        // Access the repr field (field 0) to get the raw array
                        func.instruction(&Instruction::StructGet {
                            struct_type_index: array_struct_type_idx,
                            field_index: 0, // repr is field 0
                        });
                        let u8_array_idx = self.get_array_type_index(TypeTable::U8);
                        (
                            self.array_types
                                .get(&element_type)
                                .copied()
                                .unwrap_or(u8_array_idx),
                            Some(element_type),
                            is_ref,
                            closure_type_idx,
                        )
                    } else {
                        let u8_array_idx = self.get_array_type_index(TypeTable::U8);
                        (u8_array_idx, None, false, None)
                    };

                // Now generate index and do array access
                self.generate_expr(func, index, type_table, ctx, builder);
                // For packed types (i8/u8/i16/u16), use ArrayGetS/ArrayGetU
                if let Some(elem_id) = element_type_id {
                    let elem_resolved = type_table.get(elem_id);
                    if matches!(
                        elem_resolved,
                        ResolvedType::Primitive(PrimitiveType::U8 | PrimitiveType::U16)
                    ) {
                        func.instruction(&Instruction::ArrayGetU(raw_array_type_idx));
                    } else if matches!(
                        elem_resolved,
                        ResolvedType::Primitive(PrimitiveType::I8 | PrimitiveType::I16)
                    ) {
                        func.instruction(&Instruction::ArrayGetS(raw_array_type_idx));
                    } else {
                        func.instruction(&Instruction::ArrayGet(raw_array_type_idx));
                    }
                } else {
                    func.instruction(&Instruction::ArrayGet(raw_array_type_idx));
                }
                // For reference element types, convert nullable to non-null
                // (array elements are stored as nullable refs, but we expect non-null at usage)
                if element_is_ref {
                    func.instruction(&Instruction::RefAsNonNull);
                }
                // For function types, cast structref to the canonical closure type
                if let Some(closure_struct_idx) = closure_cast_type_idx {
                    func.instruction(&Instruction::RefCastNonNull(HeapType::Concrete(
                        closure_struct_idx,
                    )));
                }
            }

            // === Block Expression ===
            TirExprKind::Block(block) => {
                // If block produces a value (not unit), use generate_block_as_expr
                if expr.type_id == TypeTable::UNIT {
                    self.generate_block(func, block, type_table, ctx, builder);
                } else {
                    self.generate_block_as_expr(
                        func,
                        block,
                        expr.type_id,
                        type_table,
                        ctx,
                        builder,
                    );
                }
            }

            // === If Expression ===
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.generate_expr(func, condition, type_table, ctx, builder);
                let result_type_id = expr.type_id;
                let result_type = self.type_id_to_valtype(type_table, result_type_id);
                func.instruction(&Instruction::If(wasm_encoder::BlockType::Result(
                    result_type,
                )));
                self.generate_block_as_expr(
                    func,
                    then_branch,
                    result_type_id,
                    type_table,
                    ctx,
                    builder,
                );
                if let Some(else_block) = else_branch {
                    func.instruction(&Instruction::Else);
                    self.generate_block_as_expr(
                        func,
                        else_block,
                        result_type_id,
                        type_table,
                        ctx,
                        builder,
                    );
                }
                func.instruction(&Instruction::End);
            }

            // === Match Expression ===
            TirExprKind::Match {
                expr: scrutinee,
                arms,
            } => {
                self.generate_match_expr(
                    func,
                    scrutinee,
                    arms,
                    expr.type_id,
                    type_table,
                    ctx,
                    builder,
                );
            }

            // === Struct Literal ===
            TirExprKind::StructLiteral {
                struct_type,
                struct_name,
                fields,
            } => {
                // Generate field values in order
                for field in fields {
                    self.generate_expr(func, &field.value, type_table, ctx, builder);
                }
                // Create struct using struct.new
                // Use the struct_type to get the correct lookup name (handles name collisions)
                let struct_info = match type_table.get(*struct_type) {
                    ResolvedType::Struct {
                        name,
                        module_source,
                        ..
                    } => self.lookup_struct_type(name, module_source),
                    ResolvedType::GenericInstance {
                        name, type_args, ..
                    } if name == "Array" && type_args.len() == 1 => {
                        // Array<T> struct literal - use the monomorphized Array struct type
                        let elem_type = type_args[0];
                        if let Some(array_struct_type_idx) =
                            self.lookup_array_struct_type(elem_type, type_table)
                        {
                            func.instruction(&Instruction::StructNew(array_struct_type_idx));
                            return;
                        } else {
                            // Fall back to simple name lookup
                            self.lookup_struct_type(struct_name, &ModuleSource::entry_point())
                        }
                    }
                    ResolvedType::GenericInstance {
                        name,
                        type_args,
                        module_source,
                    } => {
                        // Generic struct literal - look up the monomorphized struct name
                        let type_arg_names: Vec<String> = type_args
                            .iter()
                            .map(|t| type_table.mangle_type_name(*t))
                            .collect();
                        let mangled_name = mangle_generic_name(name, &type_arg_names);
                        self.lookup_struct_type(&mangled_name, module_source)
                    }
                    _ => {
                        // Fall back to simple name lookup using struct_name
                        self.lookup_struct_type(struct_name, &ModuleSource::entry_point())
                    }
                };

                if let Some(struct_info) = struct_info {
                    func.instruction(&Instruction::StructNew(struct_info.type_idx));
                } else {
                    panic!("unknown struct type: {struct_name}");
                }
            }

            // === Array Literal ===
            TirExprKind::ArrayLiteral { elements } => {
                // Push all element values onto the stack
                for elem in elements {
                    self.generate_expr(func, elem, type_table, ctx, builder);
                }

                // Get the array type from the expression's type
                if let Some(element_type_id) = type_table.as_array(expr.type_id) {
                    let u8_array_idx = self.get_array_type_index(TypeTable::U8);
                    let raw_array_type_idx = self
                        .array_types
                        .get(&element_type_id)
                        .copied()
                        .unwrap_or(u8_array_idx);

                    let array_struct_type_idx = self
                        .lookup_array_struct_type(element_type_id, type_table)
                        .expect("Array struct type should be registered");

                    // Create the raw array from values on stack
                    func.instruction(&Instruction::ArrayNewFixed {
                        array_type_index: raw_array_type_idx,
                        array_size: elements.len() as u32,
                    });

                    // Push the `used` count (same as element count for literals)
                    func.instruction(&Instruction::I32Const(elements.len() as i32));

                    // Create the Array struct with (repr, used)
                    func.instruction(&Instruction::StructNew(array_struct_type_idx));
                } else {
                    panic!(
                        "expected array type for ArrayLiteral, got {:?}",
                        type_table.get(expr.type_id)
                    );
                }
            }

            // === Tuple Literal ===
            TirExprKind::TupleLiteral { elements } => {
                if elements.is_empty() {
                    // Empty tuple is represented as i32(0)
                    func.instruction(&Instruction::I32Const(0));
                } else {
                    // Push all element values onto the stack
                    for elem in elements {
                        self.generate_expr(func, elem, type_table, ctx, builder);
                    }

                    // Get the tuple type from the expression's type
                    if let ResolvedType::Tuple(elem_type_ids) = type_table.get(expr.type_id) {
                        if let Some(type_idx) = self.get_tuple_type_idx(elem_type_ids) {
                            // Create the tuple struct
                            func.instruction(&Instruction::StructNew(type_idx));
                        } else {
                            panic!("tuple type not registered: {elem_type_ids:?}");
                        }
                    } else {
                        panic!(
                            "expected tuple type for TupleLiteral, got {:?}",
                            type_table.get(expr.type_id)
                        );
                    }
                }
            }

            // === Capture (should be transformed to FieldAccess in lower.rs) ===
            TirExprKind::Capture { .. } => {
                panic!("TirExprKind::Capture should be transformed to FieldAccess in lower.rs");
            }

            // === Closure (should be transformed to StructLiteral or ClosureToCanonical in lower.rs) ===
            TirExprKind::Closure { .. } => {
                panic!("TirExprKind::Closure should be transformed in lower.rs");
            }

            // === Indirect Call (closure or funcref) ===
            TirExprKind::IndirectCall { callee, args } => {
                // Get the callee type information and canonical closure types
                let callee_type_id = callee.type_id;
                let (fn_type_idx, closure_struct_type_idx) = if let ResolvedType::Function {
                    params,
                    return_type,
                    ..
                } = type_table.get(callee_type_id)
                {
                    // Look up canonical closure types for this function signature
                    let key = (params.clone(), *return_type);
                    if let Some((fn_idx, _, struct_idx)) =
                        self.canonical_closure_types.get(&key).cloned()
                    {
                        (fn_idx, struct_idx)
                    } else {
                        panic!(
                            "canonical closure type not found for function signature: {:?}",
                            type_table.get(callee_type_id)
                        );
                    }
                } else {
                    panic!(
                        "IndirectCall callee has non-function type: {:?}",
                        type_table.get(callee_type_id)
                    );
                };

                // Allocate a unique temporary local for this call site.
                // We use a per-type counter to ensure nested calls don't share the same local,
                // and to match the pre-allocated locals from preallocate_closure_call_locals.
                let call_id = *ctx
                    .indirect_call_counters
                    .entry(closure_struct_type_idx)
                    .or_insert(0);
                ctx.indirect_call_counters
                    .insert(closure_struct_type_idx, call_id + 1);
                let local_name = format!("__indirect_call_{closure_struct_type_idx}_{call_id}");
                let closure_local = ctx.alloc_local(
                    &local_name,
                    ValType::Ref(RefType {
                        nullable: true,
                        heap_type: HeapType::Concrete(closure_struct_type_idx),
                    }),
                );

                // Evaluate the callee expression
                self.generate_expr(func, callee, type_table, ctx, builder);

                // Store closure in temp local
                func.instruction(&Instruction::LocalTee(closure_local));

                // Extract env (field 0)
                func.instruction(&Instruction::StructGet {
                    struct_type_index: closure_struct_type_idx,
                    field_index: 0,
                });

                // Generate arguments
                self.generate_args(func, args, type_table, ctx, builder);

                // Get the closure again and extract funcref (field 1)
                func.instruction(&Instruction::LocalGet(closure_local));
                func.instruction(&Instruction::StructGet {
                    struct_type_index: closure_struct_type_idx,
                    field_index: 1,
                });

                // Call via call_ref with the function type
                func.instruction(&Instruction::CallRef(fn_type_idx));
            }

            // === Closure to Canonical Wrapper ===
            TirExprKind::ClosureToCanonical {
                functor,
                functor_id,
                target_fn_type,
            } => {
                // Generate the functor struct (pushes __Closure_N ref onto stack)
                self.generate_expr(func, functor, type_table, ctx, builder);

                // Look up the canonical wrapper function index
                // The wrapper has signature (ref struct, params...) -> result
                let wrapper_func_idx = self
                    .closure_canonical_wrappers
                    .get(functor_id)
                    .copied()
                    .unwrap_or_else(|| {
                        panic!("canonical wrapper not found for closure functor {functor_id}")
                    });

                // Get canonical closure struct type for this function signature
                let closure_struct_type_idx = if let ResolvedType::Function {
                    params,
                    return_type,
                    ..
                } = type_table.get(*target_fn_type)
                {
                    // Look up canonical closure types for this function signature
                    // These should have been registered during closure collection phase
                    let key = (params.clone(), *return_type);
                    if let Some((_, _, struct_idx)) =
                        self.canonical_closure_types.get(&key).cloned()
                    {
                        struct_idx
                    } else {
                        panic!(
                            "canonical closure type not found for ClosureToCanonical: {:?}",
                            type_table.get(*target_fn_type)
                        );
                    }
                } else {
                    panic!(
                        "ClosureToCanonical target_fn_type is not a function: {:?}",
                        type_table.get(*target_fn_type)
                    );
                };

                // Stack: functor_ref
                // Create canonical closure: (env: functor as structref, func: wrapper funcref)
                func.instruction(&Instruction::RefFunc(wrapper_func_idx));
                func.instruction(&Instruction::StructNew(closure_struct_type_idx));
            }

            // === Labeled Block Expression ===
            TirExprKind::LabeledBlock {
                label,
                block,
                result_type,
            } => {
                // Labeled block expression: produces a value via `break label: expr;`
                // Track the label so break statements can find it
                ctx.loop_info
                    .push((Some(label.clone()), 0, 0, false, Some(*result_type)));

                // Generate block with result type
                let block_type = if *result_type == TypeTable::UNIT {
                    wasm_encoder::BlockType::Empty
                } else {
                    let valtype = self.type_id_to_valtype(type_table, *result_type);
                    wasm_encoder::BlockType::Result(valtype)
                };

                func.instruction(&Instruction::Block(block_type));
                self.generate_block(func, block, type_table, ctx, builder);
                func.instruction(&Instruction::End);

                ctx.loop_info.pop();
            }
        }
    }

    /// Generate code for multiple arguments (convenience wrapper)
    fn generate_args(
        &self,
        func: &mut Function,
        args: &[TirExpr],
        type_table: &TypeTable,
        ctx: &mut FunctionContext,
        builder: &CoreModuleBuilder,
    ) {
        for arg in args {
            self.generate_expr(func, arg, type_table, ctx, builder);
        }
    }

    /// Follow newtype chain to find the ultimate base type (struct or primitive)
    fn resolve_to_ultimate_base(
        ty: ResolvedType,
        type_table: &TypeTable,
    ) -> Option<UltimateBaseType> {
        match ty {
            ResolvedType::Struct {
                name,
                module_source,
                ..
            } => Some(UltimateBaseType::Struct {
                name,
                module_source,
            }),
            ResolvedType::Primitive(prim) => Some(UltimateBaseType::Primitive(prim)),
            ResolvedType::Newtype { base_type, .. } => {
                Self::resolve_to_ultimate_base(type_table.get(base_type).clone(), type_table)
            }
            _ => None,
        }
    }

    /// Get the Wasm array type index for a given element type
    fn get_array_type_index(&self, element_type: TypeId) -> u32 {
        *self
            .array_types
            .get(&element_type)
            .expect("array type should be registered")
    }

    /// Generate code for an expression used as a statement (value is discarded).
    /// This optimizes assignment expressions to avoid the drop-tee pattern.
    fn generate_expr_as_stmt(
        &self,
        func: &mut Function,
        expr: &TirExpr,
        type_table: &TypeTable,
        ctx: &mut FunctionContext,
        builder: &CoreModuleBuilder,
    ) {
        // Check if this is an assignment expression - if so, we can optimize
        if let TirExprKind::Assign { target, value } = &expr.kind {
            self.generate_assignment_as_stmt(func, target, value, type_table, ctx, builder);
            return;
        }

        // For non-assignment expressions, generate normally and drop if needed
        self.generate_expr(func, expr, type_table, ctx, builder);
        if expr.type_id != TypeTable::UNIT && expr.type_id != TypeTable::NEVER {
            func.instruction(&Instruction::Drop);
        }
    }

    /// Generate assignment code without returning the assigned value.
    /// This avoids the drop-tee pattern where we use local.tee then immediately drop.
    fn generate_assignment_as_stmt(
        &self,
        func: &mut Function,
        target: &TirExpr,
        value: &TirExpr,
        type_table: &TypeTable,
        ctx: &mut FunctionContext,
        builder: &CoreModuleBuilder,
    ) {
        match &target.kind {
            TirExprKind::Local { index, .. } => {
                let adjusted_index = *index + ctx.local_index_offset;
                if let Some(&box_type_idx) = ctx.local_box_types.get(index) {
                    // For address-taken primitive locals, update the box
                    func.instruction(&Instruction::LocalGet(adjusted_index));
                    self.generate_expr(func, value, type_table, ctx, builder);
                    func.instruction(&Instruction::StructSet {
                        struct_type_index: box_type_idx,
                        field_index: 0,
                    });
                    // No need to push the value back - it's a statement
                } else {
                    self.generate_expr(func, value, type_table, ctx, builder);
                    // Skip copy for Move expressions (optimizer marks fresh values)
                    if self.needs_value_copy(value.type_id, type_table)
                        && !matches!(value.kind, TirExprKind::Move { .. })
                    {
                        self.generate_value_copy(func, value.type_id, type_table, ctx, builder);
                    }
                    // Use local.set instead of local.tee - value is not needed on stack
                    func.instruction(&Instruction::LocalSet(adjusted_index));
                }
            }
            TirExprKind::FieldAccess {
                expr, field_index, ..
            } => {
                let type_idx = self.get_struct_or_tuple_type_idx(expr.type_id, type_table);
                self.generate_expr(func, expr, type_table, ctx, builder);
                self.generate_expr(func, value, type_table, ctx, builder);
                func.instruction(&Instruction::StructSet {
                    struct_type_index: type_idx,
                    field_index: *field_index,
                });
                // No need to regenerate field access - it's a statement
            }
            TirExprKind::Index {
                expr: array_expr,
                index: index_expr,
            } => {
                let base_type_id = match type_table.get(array_expr.type_id) {
                    ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => *inner,
                    _ => array_expr.type_id,
                };
                let base_type = type_table.get(base_type_id);
                enum ArrayKind {
                    Array { struct_type_idx: u32 },
                    String,
                }
                let u8_array_idx = self.get_array_type_index(TypeTable::U8);
                let (raw_array_type_idx, array_kind) = if let Some(element_type) =
                    type_table.as_array(base_type_id)
                {
                    let raw_type_idx = self
                        .array_types
                        .get(&element_type)
                        .copied()
                        .unwrap_or(u8_array_idx);
                    let struct_type_idx = self
                        .lookup_array_struct_type(element_type, type_table)
                        .expect("Array struct type should be registered");
                    (raw_type_idx, ArrayKind::Array { struct_type_idx })
                } else if matches!(base_type, ResolvedType::Struct { name, .. } if name == "String")
                {
                    (u8_array_idx, ArrayKind::String)
                } else {
                    panic!("index assignment on non-array type: {base_type:?}");
                };

                self.generate_expr(func, array_expr, type_table, ctx, builder);
                match array_kind {
                    ArrayKind::Array { struct_type_idx } => {
                        func.instruction(&Instruction::StructGet {
                            struct_type_index: struct_type_idx,
                            field_index: 0,
                        });
                    }
                    ArrayKind::String => {
                        if let Some(struct_info) =
                            self.lookup_struct_type("String", &string_module_source())
                        {
                            func.instruction(&Instruction::StructGet {
                                struct_type_index: struct_info.type_idx,
                                field_index: 0,
                            });
                        }
                    }
                }
                self.generate_expr(func, index_expr, type_table, ctx, builder);
                self.generate_expr(func, value, type_table, ctx, builder);
                func.instruction(&Instruction::ArraySet(raw_array_type_idx));
                // No need to regenerate index access - it's a statement
            }
            TirExprKind::Unary {
                op: TirUnaryOp::Deref,
                expr: ref_expr,
            } => {
                let ref_type = type_table.get(ref_expr.type_id);
                if let ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) = ref_type {
                    if let ResolvedType::Primitive(prim) = type_table.get(*inner) {
                        let val_type = primitive_to_valtype(prim);
                        if let Some(box_type_idx) = self.get_box_type_idx(val_type) {
                            self.generate_expr(func, ref_expr, type_table, ctx, builder);
                            self.generate_expr(func, value, type_table, ctx, builder);
                            func.instruction(&Instruction::StructSet {
                                struct_type_index: box_type_idx,
                                field_index: 0,
                            });
                            // No need to push the value back - it's a statement
                        } else {
                            panic!("no box type for primitive in deref assignment: {prim:?}");
                        }
                    } else {
                        panic!("deref assignment for non-primitive types not yet supported");
                    }
                } else {
                    panic!("deref assignment target is not a reference type");
                }
            }
            _ => panic!("invalid assignment target in TIR"),
        }
    }

    /// Generate code for a TIR binary operation
    fn generate_binary_op(
        &self,
        func: &mut Function,
        op: TirBinaryOp,
        operand_type: TypeId,
        type_table: &TypeTable,
    ) {
        // Follow newtype chain to get the primitive type for instruction selection
        let base_type = type_table.get_ultimate_base_type(operand_type);
        let is_i64 = matches!(
            type_table.get(base_type),
            ResolvedType::Primitive(PrimitiveType::I64 | PrimitiveType::U64)
        );
        let is_f32 = matches!(
            type_table.get(base_type),
            ResolvedType::Primitive(PrimitiveType::F32)
        );
        let is_f64 = matches!(
            type_table.get(base_type),
            ResolvedType::Primitive(PrimitiveType::F64)
        );
        let is_unsigned = matches!(
            type_table.get(base_type),
            ResolvedType::Primitive(
                PrimitiveType::U8 | PrimitiveType::U16 | PrimitiveType::U32 | PrimitiveType::U64
            )
        );

        let instr = match op {
            TirBinaryOp::Add => {
                if is_f64 {
                    Instruction::F64Add
                } else if is_f32 {
                    Instruction::F32Add
                } else if is_i64 {
                    Instruction::I64Add
                } else {
                    Instruction::I32Add
                }
            }
            TirBinaryOp::Sub => {
                if is_f64 {
                    Instruction::F64Sub
                } else if is_f32 {
                    Instruction::F32Sub
                } else if is_i64 {
                    Instruction::I64Sub
                } else {
                    Instruction::I32Sub
                }
            }
            TirBinaryOp::Mul => {
                if is_f64 {
                    Instruction::F64Mul
                } else if is_f32 {
                    Instruction::F32Mul
                } else if is_i64 {
                    Instruction::I64Mul
                } else {
                    Instruction::I32Mul
                }
            }
            TirBinaryOp::Div => {
                if is_f64 {
                    Instruction::F64Div
                } else if is_f32 {
                    Instruction::F32Div
                } else if is_i64 && is_unsigned {
                    Instruction::I64DivU
                } else if is_i64 {
                    Instruction::I64DivS
                } else if is_unsigned {
                    Instruction::I32DivU
                } else {
                    Instruction::I32DivS
                }
            }
            TirBinaryOp::Mod => {
                if is_i64 && is_unsigned {
                    Instruction::I64RemU
                } else if is_i64 {
                    Instruction::I64RemS
                } else if is_unsigned {
                    Instruction::I32RemU
                } else {
                    Instruction::I32RemS
                }
            }
            TirBinaryOp::Eq => {
                if is_f64 {
                    Instruction::F64Eq
                } else if is_f32 {
                    Instruction::F32Eq
                } else if is_i64 {
                    Instruction::I64Eq
                } else {
                    Instruction::I32Eq
                }
            }
            TirBinaryOp::NotEq => {
                if is_f64 {
                    Instruction::F64Ne
                } else if is_f32 {
                    Instruction::F32Ne
                } else if is_i64 {
                    Instruction::I64Ne
                } else {
                    Instruction::I32Ne
                }
            }
            TirBinaryOp::Lt => {
                if is_f64 {
                    Instruction::F64Lt
                } else if is_f32 {
                    Instruction::F32Lt
                } else if is_i64 && is_unsigned {
                    Instruction::I64LtU
                } else if is_i64 {
                    Instruction::I64LtS
                } else if is_unsigned {
                    Instruction::I32LtU
                } else {
                    Instruction::I32LtS
                }
            }
            TirBinaryOp::LtEq => {
                if is_f64 {
                    Instruction::F64Le
                } else if is_f32 {
                    Instruction::F32Le
                } else if is_i64 && is_unsigned {
                    Instruction::I64LeU
                } else if is_i64 {
                    Instruction::I64LeS
                } else if is_unsigned {
                    Instruction::I32LeU
                } else {
                    Instruction::I32LeS
                }
            }
            TirBinaryOp::Gt => {
                if is_f64 {
                    Instruction::F64Gt
                } else if is_f32 {
                    Instruction::F32Gt
                } else if is_i64 && is_unsigned {
                    Instruction::I64GtU
                } else if is_i64 {
                    Instruction::I64GtS
                } else if is_unsigned {
                    Instruction::I32GtU
                } else {
                    Instruction::I32GtS
                }
            }
            TirBinaryOp::GtEq => {
                if is_f64 {
                    Instruction::F64Ge
                } else if is_f32 {
                    Instruction::F32Ge
                } else if is_i64 && is_unsigned {
                    Instruction::I64GeU
                } else if is_i64 {
                    Instruction::I64GeS
                } else if is_unsigned {
                    Instruction::I32GeU
                } else {
                    Instruction::I32GeS
                }
            }
            TirBinaryOp::And => Instruction::I32And,
            TirBinaryOp::Or => Instruction::I32Or,
            TirBinaryOp::BitAnd => {
                if is_i64 {
                    Instruction::I64And
                } else {
                    Instruction::I32And
                }
            }
            TirBinaryOp::BitOr => {
                if is_i64 {
                    Instruction::I64Or
                } else {
                    Instruction::I32Or
                }
            }
            TirBinaryOp::BitXor => {
                if is_i64 {
                    Instruction::I64Xor
                } else {
                    Instruction::I32Xor
                }
            }
            TirBinaryOp::Shl => {
                if is_i64 {
                    Instruction::I64Shl
                } else {
                    Instruction::I32Shl
                }
            }
            TirBinaryOp::Shr => {
                if is_i64 && is_unsigned {
                    Instruction::I64ShrU
                } else if is_i64 {
                    Instruction::I64ShrS
                } else if is_unsigned {
                    Instruction::I32ShrU
                } else {
                    Instruction::I32ShrS
                }
            }
        };
        func.instruction(&instr);
    }

    /// Generate code for a TIR unary operation
    fn generate_unary_op(
        &self,
        func: &mut Function,
        op: TirUnaryOp,
        operand_type: TypeId,
        type_table: &TypeTable,
    ) {
        let is_i64 = matches!(
            type_table.get(operand_type),
            ResolvedType::Primitive(PrimitiveType::I64 | PrimitiveType::U64)
        );
        let is_f32 = matches!(
            type_table.get(operand_type),
            ResolvedType::Primitive(PrimitiveType::F32)
        );
        let is_f64 = matches!(
            type_table.get(operand_type),
            ResolvedType::Primitive(PrimitiveType::F64)
        );

        match op {
            TirUnaryOp::Neg => {
                if is_f64 {
                    func.instruction(&Instruction::F64Neg);
                } else if is_f32 {
                    func.instruction(&Instruction::F32Neg);
                } else if is_i64 {
                    // value is already on stack, negate by multiplying by -1
                    func.instruction(&Instruction::I64Const(-1));
                    func.instruction(&Instruction::I64Mul);
                } else {
                    // value is already on stack, negate by multiplying by -1
                    func.instruction(&Instruction::I32Const(-1));
                    func.instruction(&Instruction::I32Mul);
                }
            }
            TirUnaryOp::Not => {
                func.instruction(&Instruction::I32Eqz);
            }
            TirUnaryOp::BitNot => {
                if is_i64 {
                    func.instruction(&Instruction::I64Const(-1));
                    func.instruction(&Instruction::I64Xor);
                } else {
                    func.instruction(&Instruction::I32Const(-1));
                    func.instruction(&Instruction::I32Xor);
                }
            }
            TirUnaryOp::Ref | TirUnaryOp::MutRef => {
                // For primitives (including newtypes of primitives), box the value in a single-field struct
                // For GC types (structs, arrays, tuples), references are transparent
                // Follow newtypes to find the ultimate primitive base type
                let ultimate_operand = type_table.get_ultimate_base_type(operand_type);
                if let ResolvedType::Primitive(prim) = type_table.get(ultimate_operand) {
                    let val_type = primitive_to_valtype(prim);
                    if let Some(box_type_idx) = self.get_box_type_idx(val_type) {
                        func.instruction(&Instruction::StructNew(box_type_idx));
                    }
                    // else: no box type for this primitive, treat as transparent
                }
                // For non-primitives (structs, arrays, tuples), no operation needed
            }
            TirUnaryOp::Deref => {
                // For references to primitives (including newtypes of primitives), unbox by extracting from the box struct
                // For references to GC types, references are transparent
                if let ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) =
                    type_table.get(operand_type)
                {
                    // Follow newtypes to find the ultimate primitive base type
                    let ultimate_inner = type_table.get_ultimate_base_type(*inner);
                    if let ResolvedType::Primitive(prim) = type_table.get(ultimate_inner) {
                        let val_type = primitive_to_valtype(prim);
                        if let Some(box_type_idx) = self.get_box_type_idx(val_type) {
                            func.instruction(&Instruction::StructGet {
                                struct_type_index: box_type_idx,
                                field_index: 0,
                            });
                        }
                    }
                }
                // For non-primitive references, no operation needed
            }
        }
    }

    /// Generate code for a TIR type cast
    fn generate_cast(
        &self,
        func: &mut Function,
        from_type: TypeId,
        to_type: TypeId,
        type_table: &TypeTable,
    ) {
        let from = type_table.get(from_type);
        let to = type_table.get(to_type);

        match (from, to) {
            // i32 -> i64 (signed extend)
            (
                ResolvedType::Primitive(PrimitiveType::I32),
                ResolvedType::Primitive(PrimitiveType::I64),
            ) => {
                func.instruction(&Instruction::I64ExtendI32S);
            }
            // i32 -> u64 (unsigned extend)
            (
                ResolvedType::Primitive(PrimitiveType::I32),
                ResolvedType::Primitive(PrimitiveType::U64),
            ) => {
                func.instruction(&Instruction::I64ExtendI32U);
            }
            // u32 -> i64 (unsigned extend)
            (
                ResolvedType::Primitive(PrimitiveType::U32),
                ResolvedType::Primitive(PrimitiveType::I64),
            ) => {
                func.instruction(&Instruction::I64ExtendI32U);
            }
            // u32 -> u64 (unsigned extend)
            (
                ResolvedType::Primitive(PrimitiveType::U32),
                ResolvedType::Primitive(PrimitiveType::U64),
            ) => {
                func.instruction(&Instruction::I64ExtendI32U);
            }
            // i64 -> i32 (truncate)
            (
                ResolvedType::Primitive(PrimitiveType::I64),
                ResolvedType::Primitive(PrimitiveType::I32),
            ) => {
                func.instruction(&Instruction::I32WrapI64);
            }
            // u64 -> i32 (truncate)
            (
                ResolvedType::Primitive(PrimitiveType::U64),
                ResolvedType::Primitive(PrimitiveType::I32),
            ) => {
                func.instruction(&Instruction::I32WrapI64);
            }
            // i64 -> u32 (truncate)
            (
                ResolvedType::Primitive(PrimitiveType::I64),
                ResolvedType::Primitive(PrimitiveType::U32),
            ) => {
                func.instruction(&Instruction::I32WrapI64);
            }
            // u64 -> u32 (truncate)
            (
                ResolvedType::Primitive(PrimitiveType::U64),
                ResolvedType::Primitive(PrimitiveType::U32),
            ) => {
                func.instruction(&Instruction::I32WrapI64);
            }
            // i32 -> f64
            (
                ResolvedType::Primitive(PrimitiveType::I32),
                ResolvedType::Primitive(PrimitiveType::F64),
            ) => {
                func.instruction(&Instruction::F64ConvertI32S);
            }
            // i32 -> f32
            (
                ResolvedType::Primitive(PrimitiveType::I32),
                ResolvedType::Primitive(PrimitiveType::F32),
            ) => {
                func.instruction(&Instruction::F32ConvertI32S);
            }
            // f64 -> i32 (truncate)
            (
                ResolvedType::Primitive(PrimitiveType::F64),
                ResolvedType::Primitive(PrimitiveType::I32),
            ) => {
                func.instruction(&Instruction::I32TruncF64S);
            }
            // f32 -> i32 (truncate)
            (
                ResolvedType::Primitive(PrimitiveType::F32),
                ResolvedType::Primitive(PrimitiveType::I32),
            ) => {
                func.instruction(&Instruction::I32TruncF32S);
            }
            // f32 -> f64
            (
                ResolvedType::Primitive(PrimitiveType::F32),
                ResolvedType::Primitive(PrimitiveType::F64),
            ) => {
                func.instruction(&Instruction::F64PromoteF32);
            }
            // f64 -> f32
            (
                ResolvedType::Primitive(PrimitiveType::F64),
                ResolvedType::Primitive(PrimitiveType::F32),
            ) => {
                func.instruction(&Instruction::F32DemoteF64);
            }
            // i64 -> f64
            (
                ResolvedType::Primitive(PrimitiveType::I64),
                ResolvedType::Primitive(PrimitiveType::F64),
            ) => {
                func.instruction(&Instruction::F64ConvertI64S);
            }
            // u64 -> f64
            (
                ResolvedType::Primitive(PrimitiveType::U64),
                ResolvedType::Primitive(PrimitiveType::F64),
            ) => {
                func.instruction(&Instruction::F64ConvertI64U);
            }
            // i64 -> f32
            (
                ResolvedType::Primitive(PrimitiveType::I64),
                ResolvedType::Primitive(PrimitiveType::F32),
            ) => {
                func.instruction(&Instruction::F32ConvertI64S);
            }
            // u64 -> f32
            (
                ResolvedType::Primitive(PrimitiveType::U64),
                ResolvedType::Primitive(PrimitiveType::F32),
            ) => {
                func.instruction(&Instruction::F32ConvertI64U);
            }
            // f64 -> i64
            (
                ResolvedType::Primitive(PrimitiveType::F64),
                ResolvedType::Primitive(PrimitiveType::I64),
            ) => {
                func.instruction(&Instruction::I64TruncF64S);
            }
            // f64 -> u64
            (
                ResolvedType::Primitive(PrimitiveType::F64),
                ResolvedType::Primitive(PrimitiveType::U64),
            ) => {
                func.instruction(&Instruction::I64TruncF64U);
            }
            // f32 -> i64
            (
                ResolvedType::Primitive(PrimitiveType::F32),
                ResolvedType::Primitive(PrimitiveType::I64),
            ) => {
                func.instruction(&Instruction::I64TruncF32S);
            }
            // f32 -> u64
            (
                ResolvedType::Primitive(PrimitiveType::F32),
                ResolvedType::Primitive(PrimitiveType::U64),
            ) => {
                func.instruction(&Instruction::I64TruncF32U);
            }
            // u64 -> i64 (no-op, same representation)
            (
                ResolvedType::Primitive(PrimitiveType::U64),
                ResolvedType::Primitive(PrimitiveType::I64),
            ) => {
                // No instruction needed
            }
            // i64 -> u64 (no-op, same representation)
            (
                ResolvedType::Primitive(PrimitiveType::I64),
                ResolvedType::Primitive(PrimitiveType::U64),
            ) => {
                // No instruction needed
            }
            // u32 -> i32 (no-op, same representation)
            (
                ResolvedType::Primitive(PrimitiveType::U32),
                ResolvedType::Primitive(PrimitiveType::I32),
            ) => {
                // No instruction needed
            }
            // i32 -> u32 (no-op, same representation)
            (
                ResolvedType::Primitive(PrimitiveType::I32),
                ResolvedType::Primitive(PrimitiveType::U32),
            ) => {
                // No instruction needed
            }
            // bool -> i64 (unsigned extend, bool is 0 or 1)
            (
                ResolvedType::Primitive(PrimitiveType::Bool),
                ResolvedType::Primitive(PrimitiveType::I64),
            ) => {
                func.instruction(&Instruction::I64ExtendI32U);
            }
            // bool -> u64 (unsigned extend, bool is 0 or 1)
            (
                ResolvedType::Primitive(PrimitiveType::Bool),
                ResolvedType::Primitive(PrimitiveType::U64),
            ) => {
                func.instruction(&Instruction::I64ExtendI32U);
            }
            // bool -> i32 (no-op, bool is stored as i32)
            (
                ResolvedType::Primitive(PrimitiveType::Bool),
                ResolvedType::Primitive(PrimitiveType::I32),
            ) => {
                // No instruction needed
            }
            // bool -> u32 (no-op, bool is stored as i32)
            (
                ResolvedType::Primitive(PrimitiveType::Bool),
                ResolvedType::Primitive(PrimitiveType::U32),
            ) => {
                // No instruction needed
            }
            // Same type - no conversion needed
            _ if from_type == to_type => {}
            // Other conversions - placeholder
            _ => {
                // TODO: Handle more type conversions
            }
        }
    }

    /// Generate code for a TIR block
    fn generate_block(
        &self,
        func: &mut Function,
        block: &TirBlock,
        type_table: &TypeTable,
        ctx: &mut FunctionContext,
        builder: &CoreModuleBuilder,
    ) {
        for stmt in &block.stmts {
            self.generate_stmt(func, stmt, type_table, ctx, builder);
        }
    }

    /// Generate code for a TIR block as an expression (keeps last expression value on stack)
    fn generate_block_as_expr(
        &self,
        func: &mut Function,
        block: &TirBlock,
        result_type: TypeId,
        type_table: &TypeTable,
        ctx: &mut FunctionContext,
        builder: &CoreModuleBuilder,
    ) {
        let len = block.stmts.len();
        for (i, stmt) in block.stmts.iter().enumerate() {
            let is_last = i == len - 1;
            if is_last && result_type != TypeTable::UNIT {
                // For the last statement in an expression block, keep the value on stack
                match &stmt.kind {
                    TirStmtKind::Expr(expr) => {
                        self.generate_expr(func, expr, type_table, ctx, builder);
                    }
                    TirStmtKind::If {
                        condition,
                        then_block,
                        else_block: Some(else_block),
                    } => {
                        // Generate if statement as expression (with result type)
                        self.generate_expr(func, condition, type_table, ctx, builder);
                        let result_valtype = self.type_id_to_valtype(type_table, result_type);
                        func.instruction(&Instruction::If(wasm_encoder::BlockType::Result(
                            result_valtype,
                        )));
                        self.generate_block_as_expr(
                            func,
                            then_block,
                            result_type,
                            type_table,
                            ctx,
                            builder,
                        );
                        func.instruction(&Instruction::Else);
                        self.generate_block_as_expr(
                            func,
                            else_block,
                            result_type,
                            type_table,
                            ctx,
                            builder,
                        );
                        func.instruction(&Instruction::End);
                    }
                    TirStmtKind::IfPattern { .. } => {
                        // All IfPattern statements should be lowered to Let + If by the lower phase.
                        // If we reach here, it's a compiler bug.
                        panic!("IfPattern should be lowered before codegen");
                    }
                    _ => {
                        // Not a statement that can produce a value - generate normally
                        self.generate_stmt(func, stmt, type_table, ctx, builder);
                        // Block expects a value but last stmt isn't an expression,
                        // this is a type error that should've been caught by the resolver
                    }
                }
            } else {
                self.generate_stmt(func, stmt, type_table, ctx, builder);
            }
        }

        // If block is empty and we need a result, push unit (unreachable in practice)
        // Push a default value for the expected type (shouldn't happen with valid code)
        assert!(
            !(block.stmts.is_empty() && result_type != TypeTable::UNIT),
            "Empty block cannot produce non-unit value"
        );
    }

    /// Generate code for a TIR statement
    fn generate_stmt(
        &self,
        func: &mut Function,
        stmt: &TirStmt,
        type_table: &TypeTable,
        ctx: &mut FunctionContext,
        builder: &CoreModuleBuilder,
    ) {
        match &stmt.kind {
            TirStmtKind::Let {
                name: _,
                local_index,
                type_id,
                value,
                ..
            } => {
                // Generate the initializer
                self.generate_expr(func, value, type_table, ctx, builder);

                // Add implicit type promotion if needed (e.g., i32 literal to i64 local)
                let value_type = value.type_id;
                if value_type != *type_id {
                    // Check for i32 -> i64 promotion (common with integer literals)
                    let is_i32_to_i64 = matches!(
                        (type_table.get(value_type), type_table.get(*type_id)),
                        (
                            ResolvedType::Primitive(PrimitiveType::I32),
                            ResolvedType::Primitive(PrimitiveType::I64 | PrimitiveType::U64)
                        )
                    );
                    if is_i32_to_i64 {
                        func.instruction(&Instruction::I64ExtendI32S);
                    }
                }

                // Apply value copy for struct/array/tuple types (value semantics)
                // Skip for Move-wrapped values - the optimizer marks fresh values with Move
                if self.needs_value_copy(value.type_id, type_table)
                    && !matches!(value.kind, TirExprKind::Move { .. })
                {
                    self.generate_value_copy(func, value.type_id, type_table, ctx, builder);
                }

                // For address-taken primitives, wrap in a box
                if let Some(&box_type_idx) = ctx.local_box_types.get(local_index) {
                    func.instruction(&Instruction::StructNew(box_type_idx));
                }

                // Store to local (apply offset for closure functions)
                let adjusted_index = *local_index + ctx.local_index_offset;
                func.instruction(&Instruction::LocalSet(adjusted_index));
            }

            TirStmtKind::LetPattern {
                pattern,
                is_mut: _,
                value,
            } => {
                // Optimization: For multi-value builtin calls (e.g., i64_add128),
                // directly bind stack values to locals without creating a tuple struct.
                // This avoids heap allocation and struct.get for each element.
                if self.try_generate_multivalue_builtin_destructure(
                    func, pattern, value, type_table, ctx, builder,
                ) {
                    // Optimization succeeded, skip normal path
                } else {
                    // Generate the value expression (should be a tuple)
                    self.generate_expr(func, value, type_table, ctx, builder);

                    // Apply value copy for tuple types (value semantics)
                    // Skip for Move-wrapped values - the optimizer marks fresh values with Move
                    if self.needs_value_copy(value.type_id, type_table)
                        && !matches!(value.kind, TirExprKind::Move { .. })
                    {
                        self.generate_value_copy(func, value.type_id, type_table, ctx, builder);
                    }

                    // Destructure the tuple according to the pattern
                    self.generate_let_pattern_binding(
                        func,
                        pattern,
                        value.type_id,
                        type_table,
                        ctx,
                        builder,
                    );
                }
            }

            TirStmtKind::Expr(expr) => {
                // Use optimized statement generation to avoid drop-tee pattern
                self.generate_expr_as_stmt(func, expr, type_table, ctx, builder);
            }

            TirStmtKind::Return { value } => {
                if ctx.is_async_export {
                    // For async exports, call task-return with the result value
                    if ctx.has_http_handler_export {
                        // Service world: result<response, error-code>
                        // Generate the return expression but drop it for now
                        if let Some(expr) = value {
                            self.generate_expr(func, expr, type_table, ctx, builder);
                            func.instruction(&Instruction::Drop);
                        }

                        // Try creating HTTP 200 response:
                        // 1. Create headers
                        // 2. Create trailers future (rx, tx)
                        // 3. Call response.new(rx) - this starts the reader!
                        // 4. Write None to trailers future (reader should be ready now)
                        // 5. Return Ok(response)

                        // 1. Create headers
                        let fields_constructor_idx = builder.func_idx("http-fields-constructor");
                        func.instruction(&Instruction::Call(fields_constructor_idx));
                        let headers_handle = ctx.alloc_local("_headers_handle", ValType::I32);
                        func.instruction(&Instruction::LocalSet(headers_handle));

                        // 2. Create trailers future
                        let future_new_idx = builder.func_idx("future-new");
                        func.instruction(&Instruction::Call(future_new_idx));
                        let future_local = ctx.alloc_local("_http_future", ValType::I64);
                        func.instruction(&Instruction::LocalSet(future_local));
                        // Extract rx (low 32 bits)
                        func.instruction(&Instruction::LocalGet(future_local));
                        func.instruction(&Instruction::I32WrapI64);
                        let trailers_rx = ctx.alloc_local("_trailers_rx", ValType::I32);
                        func.instruction(&Instruction::LocalSet(trailers_rx));
                        // Extract tx (high 32 bits)
                        func.instruction(&Instruction::LocalGet(future_local));
                        func.instruction(&Instruction::I64Const(32));
                        func.instruction(&Instruction::I64ShrU);
                        func.instruction(&Instruction::I32WrapI64);
                        let trailers_tx = ctx.alloc_local("_trailers_tx", ValType::I32);
                        func.instruction(&Instruction::LocalSet(trailers_tx));

                        // 3. Call response.new FIRST (this starts the reader!)
                        // response.new returns tuple: [response_handle, transmission_future]
                        let response_new_idx = builder.func_idx("http-response-new");
                        func.instruction(&Instruction::LocalGet(headers_handle)); // headers
                        func.instruction(&Instruction::I32Const(0)); // body discriminant = None
                        func.instruction(&Instruction::I32Const(0)); // body stream handle
                        func.instruction(&Instruction::LocalGet(trailers_rx)); // trailers future rx
                        func.instruction(&Instruction::I32Const(128)); // out_ptr
                        func.instruction(&Instruction::Call(response_new_idx));

                        // 4. Read response handle from offset 128 (before task.return)
                        func.instruction(&Instruction::I32Const(128));
                        func.instruction(&Instruction::I32Load(MemArg {
                            offset: 0,
                            align: 2,
                            memory_index: 0,
                        }));
                        let response_handle = ctx.alloc_local("_response_handle", ValType::I32);
                        func.instruction(&Instruction::LocalSet(response_handle));

                        // 5. Return Ok(response) via task-return
                        func.instruction(&Instruction::I32Const(0)); // Ok discriminant
                        func.instruction(&Instruction::LocalGet(response_handle));
                        func.instruction(&Instruction::I32Const(0)); // padding
                        func.instruction(&Instruction::I64Const(0)); // padding
                        func.instruction(&Instruction::I32Const(0)); // padding
                        func.instruction(&Instruction::I32Const(0)); // padding
                        func.instruction(&Instruction::I32Const(0)); // padding
                        func.instruction(&Instruction::I32Const(0)); // padding
                        let task_ret = builder.func_idx("task-return");
                        func.instruction(&Instruction::Call(task_ret));

                        // 6. Write None (no trailers) to the trailers future
                        // This is post-return execution - Component Model allows
                        // code to run after task.return
                        //
                        // future-write payload: result<option<fields>, error-code>
                        // - Ok(None) = 0 (result Ok), 0 (option None)
                        // The payload is at memory offset, we'll use offset 256
                        func.instruction(&Instruction::I32Const(256)); // offset for payload
                        func.instruction(&Instruction::I32Const(0)); // Ok discriminant
                        func.instruction(&Instruction::I32Store(MemArg {
                            offset: 0,
                            align: 2,
                            memory_index: 0,
                        }));
                        func.instruction(&Instruction::I32Const(260)); // offset for option
                        func.instruction(&Instruction::I32Const(0)); // None discriminant
                        func.instruction(&Instruction::I32Store(MemArg {
                            offset: 0,
                            align: 2,
                            memory_index: 0,
                        }));
                        // Call future-write(tx, payload_ptr)
                        func.instruction(&Instruction::LocalGet(trailers_tx));
                        func.instruction(&Instruction::I32Const(256)); // payload ptr
                        let future_write_idx = builder.func_idx("future-write");
                        func.instruction(&Instruction::Call(future_write_idx));
                        // Drop the return code (we don't check it)
                        func.instruction(&Instruction::Drop);

                        func.instruction(&Instruction::Return);
                    } else {
                        // Command world: result<_, _> needs just (i32)
                        func.instruction(&Instruction::I32Const(0)); // Ok discriminant
                        let task_return_idx = builder.func_idx("task-return");
                        func.instruction(&Instruction::Call(task_return_idx));
                        func.instruction(&Instruction::Return);
                    }
                } else {
                    // Normal function return
                    if let Some(expr) = value {
                        self.generate_expr(func, expr, type_table, ctx, builder);
                    }
                    func.instruction(&Instruction::Return);
                }
            }

            TirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                self.generate_expr(func, condition, type_table, ctx, builder);
                // Record branch hint at the current offset (before emitting the if instruction)
                ctx.consume_branch_hint(func.byte_len() as u32);
                func.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
                // If creates a block level - increment extra depth if we're inside a loop
                if let Some((_, extra, _, _, _)) = ctx.loop_info.last_mut() {
                    *extra += 1;
                }
                self.generate_block(func, then_block, type_table, ctx, builder);
                if let Some(else_blk) = else_block {
                    func.instruction(&Instruction::Else);
                    // Else branch is at the same depth as then branch
                    self.generate_block(func, else_blk, type_table, ctx, builder);
                }
                if let Some((_, extra, _, _, _)) = ctx.loop_info.last_mut() {
                    *extra -= 1;
                }
                func.instruction(&Instruction::End);
            }

            TirStmtKind::Loop { body } => {
                // Push new loop context: (extra_depth=0, break_offset=1, no result type)
                ctx.loop_info.push((None, 0, 1, true, None));

                func.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
                func.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));

                self.generate_block(func, body, type_table, ctx, builder);

                // Continue loop
                let (_, extra, _, _, _) = *ctx.loop_info.last().unwrap();
                func.instruction(&Instruction::Br(extra));

                func.instruction(&Instruction::End); // End loop
                func.instruction(&Instruction::End); // End block

                ctx.loop_info.pop();
            }

            TirStmtKind::Break { label, value } => {
                // Find the target in loop_info
                if let Some(target_label) = label {
                    // Find the labeled block/loop with this label
                    let mut found = false;
                    let mut depth: u32 = 0;
                    let mut blocks_passed: u32 = 0; // Total wasm blocks passed over

                    for (i, (lbl, _extra, break_offset, _is_loop, _result_type)) in
                        ctx.loop_info.iter().rev().enumerate()
                    {
                        if lbl.as_ref() == Some(target_label) {
                            // Found the target
                            // Depth is: blocks from entries we passed + target's break_offset + total_extra
                            // Each construct creates (break_offset + 1) wasm blocks total
                            let mut total_extra: u32 = 0;
                            for (j, (_, e, _, _, _)) in ctx.loop_info.iter().rev().enumerate() {
                                if j > i {
                                    break;
                                }
                                total_extra += *e;
                            }
                            depth = blocks_passed + *break_offset + total_extra;
                            found = true;
                            break;
                        }
                        // Add this entry's total block count (break_offset + 1)
                        // - while/loop: break_offset=1 -> 2 blocks (exit + loop)
                        // - for/for-of: break_offset=2 -> 3 blocks (exit + loop + body)
                        // - labeled block: break_offset=0 -> 1 block
                        blocks_passed += *break_offset + 1;
                    }
                    assert!(found, "labeled break target not found: {target_label}");

                    // If breaking with a value, generate the value expression first
                    if let Some(val) = value {
                        self.generate_expr(func, val, type_table, ctx, builder);
                    }

                    func.instruction(&Instruction::Br(depth));
                } else {
                    // Unlabeled break - find the innermost loop
                    if let Some((_, extra, break_offset, is_loop, _)) = ctx.loop_info.last() {
                        assert!(
                            *is_loop,
                            "unlabeled break inside labeled block but not in a loop"
                        );
                        // Break to outer block: break_offset + extra_depth
                        func.instruction(&Instruction::Br(break_offset + extra));
                    } else {
                        // No enclosing loop - this should have been caught earlier
                        panic!("break outside of loop");
                    }
                }
            }

            TirStmtKind::Continue => {
                // Find the innermost loop (not just labeled block)
                let mut found = false;
                let mut depth: u32 = 0;
                let mut extra_from_nested: u32 = 0;
                for (_lbl, extra, _break_offset, is_loop, _) in ctx.loop_info.iter().rev() {
                    extra_from_nested += *extra;
                    if *is_loop {
                        // Found a loop - continue to it
                        // For continue, we jump to the loop header (extra_depth from nested blocks)
                        depth = extra_from_nested - *extra + *extra; // Just the extra of all nested
                        found = true;
                        break;
                    }
                    // Not a loop, add its block depth (1 for the block itself)
                    extra_from_nested += 1;
                }
                if found {
                    func.instruction(&Instruction::Br(depth));
                } else if let Some((_, extra, _, _, _)) = ctx.loop_info.last() {
                    // Fallback to old behavior
                    func.instruction(&Instruction::Br(*extra));
                } else {
                    panic!("continue outside of loop");
                }
            }

            TirStmtKind::LabeledBlock { label, block } => {
                // Generate a wasm block with the label tracked
                // break_offset is 0 because br 0 goes to this block's exit
                // No result type since this is a statement (not expression)
                ctx.loop_info.push((Some(label.clone()), 0, 0, false, None));
                func.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
                self.generate_block(func, block, type_table, ctx, builder);
                func.instruction(&Instruction::End);
                ctx.loop_info.pop();
            }

            TirStmtKind::IfPattern { .. } => {
                // All IfPattern statements should be lowered to Let + If by the lower phase.
                // If we reach here, it's a compiler bug.
                panic!("IfPattern should be lowered before codegen");
            }
        }
    }

    /// Generate code for let pattern binding (tuple destructuring)
    /// The tuple value should already be on the stack
    fn generate_let_pattern_binding(
        &self,
        func: &mut Function,
        pattern: &TirPattern,
        tuple_type_id: TypeId,
        type_table: &TypeTable,
        ctx: &mut FunctionContext,
        _builder: &CoreModuleBuilder,
    ) {
        match pattern {
            TirPattern::Tuple(patterns) => {
                // Get element types from the tuple
                let elem_types = if let ResolvedType::Tuple(types) = type_table.get(tuple_type_id) {
                    types.clone()
                } else {
                    // Error: expected tuple type, but we'll handle gracefully
                    vec![TypeTable::UNKNOWN; patterns.len()]
                };

                // Get the tuple type index
                let tuple_type_idx = if let Some(idx) = self.get_tuple_type_idx(&elem_types) {
                    idx
                } else {
                    panic!("tuple type not registered for destructuring");
                };

                // Get the ValType for the tuple to allocate a temporary local
                let tuple_val_type = self.type_id_to_valtype(type_table, tuple_type_id);

                // Cast to the specific tuple type if the value on stack is a generic struct ref
                // This is needed when extracting tuple payloads from variants
                func.instruction(&Instruction::RefCastNonNull(HeapType::Concrete(
                    tuple_type_idx,
                )));

                // Store the tuple in a temporary local (using pre-allocated local)
                let local_name = ctx.next_let_pattern_local_name();
                let temp_local = ctx.alloc_local(&local_name, tuple_val_type);
                func.instruction(&Instruction::LocalSet(temp_local));

                // Bind each element
                for (i, (sub_pattern, elem_type)) in patterns
                    .iter()
                    .zip(
                        elem_types
                            .iter()
                            .chain(std::iter::repeat(&TypeTable::UNKNOWN)),
                    )
                    .enumerate()
                {
                    match sub_pattern {
                        TirPattern::Binding {
                            local_index,
                            type_id: _,
                            ..
                        } => {
                            // Get tuple element and store in local
                            func.instruction(&Instruction::LocalGet(temp_local));
                            func.instruction(&Instruction::StructGet {
                                struct_type_index: tuple_type_idx,
                                field_index: i as u32,
                            });

                            // Apply value copy if needed
                            if self.needs_value_copy(*elem_type, type_table) {
                                self.generate_value_copy(
                                    func, *elem_type, type_table, ctx, _builder,
                                );
                            }

                            let adjusted_index = *local_index + ctx.local_index_offset;
                            func.instruction(&Instruction::LocalSet(adjusted_index));
                        }
                        TirPattern::Tuple(_) => {
                            // Nested tuple destructuring
                            func.instruction(&Instruction::LocalGet(temp_local));
                            func.instruction(&Instruction::StructGet {
                                struct_type_index: tuple_type_idx,
                                field_index: i as u32,
                            });

                            // Recursively bind the nested tuple
                            self.generate_let_pattern_binding(
                                func,
                                sub_pattern,
                                *elem_type,
                                type_table,
                                ctx,
                                _builder,
                            );
                        }
                        TirPattern::Wildcard => {
                            // Wildcard - don't bind anything
                        }
                        TirPattern::Literal(_) | TirPattern::Variant { .. } => {
                            // These patterns are not valid in let statements
                            // Should have been caught by resolver
                        }
                    }
                }
            }
            TirPattern::Binding {
                local_index,
                type_id: _,
                ..
            } => {
                // Single binding - just store the value directly
                let adjusted_index = *local_index + ctx.local_index_offset;
                func.instruction(&Instruction::LocalSet(adjusted_index));
            }
            TirPattern::Wildcard => {
                // Wildcard - just drop the value
                func.instruction(&Instruction::Drop);
            }
            TirPattern::Literal(_) | TirPattern::Variant { .. } => {
                // These patterns are not valid in let statements
                // Drop the value
                func.instruction(&Instruction::Drop);
            }
        }
    }

    /// Try to generate optimized code for multi-value builtin calls with destructuring.
    ///
    /// Multi-value Wasm instructions (like i64.add128, `i64.mul_wide_u`) return multiple values
    /// on the stack. Normally these are wrapped in a tuple struct and then destructured.
    /// This optimization bypasses the tuple struct entirely when the pattern is a flat tuple
    /// of bindings/wildcards, directly binding stack values to locals.
    ///
    /// The optimization is triggered when:
    /// 1. The pattern is a flat tuple with only Binding or Wildcard patterns
    /// 2. The value expression returns a tuple type (detected via metadata)
    /// 3. The pattern length matches the tuple element count
    ///
    /// Returns `true` if the optimization was applied, `false` otherwise.
    fn try_generate_multivalue_builtin_destructure(
        &self,
        func: &mut Function,
        pattern: &TirPattern,
        value: &TirExpr,
        type_table: &TypeTable,
        ctx: &mut FunctionContext,
        builder: &CoreModuleBuilder,
    ) -> bool {
        // Check if pattern is a flat tuple with only Binding or Wildcard patterns
        let patterns = match pattern {
            TirPattern::Tuple(patterns) => patterns,
            _ => return false,
        };

        // Verify all sub-patterns are simple bindings or wildcards
        for p in patterns {
            match p {
                TirPattern::Binding { .. } | TirPattern::Wildcard => {}
                _ => return false, // Nested tuples or other patterns - can't optimize
            }
        }

        // Unwrap Move wrapper if present
        let inner_value = match &value.kind {
            TirExprKind::Move { expr } => expr.as_ref(),
            _ => value,
        };

        // Check if value is a builtin call (only builtins can return multi-value on Wasm stack)
        let is_builtin_call = match &inner_value.kind {
            TirExprKind::Call { func: func_ref, .. } => {
                if let crate::tir::FunctionRef::External { module_source, .. } = func_ref {
                    matches!(module_source, ModuleSource::Core { name } if name == "builtin")
                } else {
                    false
                }
            }
            _ => false,
        };

        if !is_builtin_call {
            return false;
        }

        // Check if return type is a tuple (multi-value return)
        let elem_types = match type_table.get(inner_value.type_id) {
            ResolvedType::Tuple(types) => types,
            _ => return false,
        };

        // Verify pattern length matches tuple element count
        if patterns.len() != elem_types.len() {
            return false;
        }

        // Generate the expression with skip_tuple_wrap flag set
        // This tells builtin codegen to skip struct.new for tuple returns
        ctx.skip_tuple_wrap = true;
        self.generate_expr(func, inner_value, type_table, ctx, builder);
        ctx.skip_tuple_wrap = false;

        // Bind stack values to locals in reverse order (LIFO)
        // Stack after multi-value instruction: [..., elem0, elem1, ..., elemN-1]
        // elemN-1 is on top, so we need to set locals in reverse pattern order
        for sub_pattern in patterns.iter().rev() {
            match sub_pattern {
                TirPattern::Binding { local_index, .. } => {
                    let adjusted_index = *local_index + ctx.local_index_offset;
                    func.instruction(&Instruction::LocalSet(adjusted_index));
                }
                TirPattern::Wildcard => {
                    func.instruction(&Instruction::Drop);
                }
                _ => unreachable!("verified above"),
            }
        }

        true
    }

    // =========================================================================
    // br_table optimization for integer match expressions
    // =========================================================================

    /// Build `br_table` targets array for a dense integer match.
    ///
    /// Given:
    /// - `value_to_arm_map`: for each index in range [0, range), which arm index to jump to
    /// - `num_arms`: total number of match arms
    /// - `default_arm`: optional index of the wildcard/default arm
    ///
    /// Returns:
    /// - `targets`: Vec of branch depths for `br_table`
    /// - `default_target`: branch depth for out-of-range values
    ///
    /// Block structure (from innermost to outermost):
    /// - dispatch block (depth 0)
    /// - arm[num_arms-1] block (depth 1)
    /// - arm[num_arms-2] block (depth 2)
    /// - ...
    /// - arm[0] block (depth `num_arms`)
    /// - done block (depth `num_arms` + 1)
    fn build_br_table_targets(
        value_to_arm_map: &[usize],
        num_arms: usize,
        default_arm: Option<usize>,
    ) -> (Vec<u32>, u32) {
        let mut targets: Vec<u32> = Vec::with_capacity(value_to_arm_map.len());

        for &arm_idx in value_to_arm_map {
            if arm_idx < num_arms {
                // Jump to arm block: depth = num_arms - arm_idx
                targets.push((num_arms - arm_idx) as u32);
            } else {
                // No matching arm - jump to dispatch block (depth 0)
                // Will fall through to unreachable
                targets.push(0);
            }
        }

        // Default target for out-of-range values
        let default_target = if let Some(def_idx) = default_arm {
            (num_arms - def_idx) as u32
        } else {
            0 // Will hit unreachable after dispatch block
        };

        (targets, default_target)
    }

    /// Analyze match arms to determine if `br_table` optimization is applicable
    fn analyze_for_br_table(
        arms: &[TirMatchArm],
        scrutinee_type: &ResolvedType,
    ) -> Option<BrTableAnalysis> {
        // Only applicable to integer types
        let is_i64 = match scrutinee_type {
            ResolvedType::Primitive(PrimitiveType::I64 | PrimitiveType::U64) => true,
            ResolvedType::Primitive(
                PrimitiveType::I32
                | PrimitiveType::U32
                | PrimitiveType::I16
                | PrimitiveType::U16
                | PrimitiveType::I8
                | PrimitiveType::U8,
            ) => false,
            _ => return None,
        };

        let mut value_to_arm: Vec<(i64, usize)> = Vec::new();
        let mut default_arm: Option<usize> = None;

        for (arm_idx, arm) in arms.iter().enumerate() {
            match &arm.pattern {
                TirPattern::Literal(TirLiteralPattern::I128(v)) => {
                    value_to_arm.push((*v as i64, arm_idx));
                }
                TirPattern::Literal(TirLiteralPattern::U128(v)) => {
                    value_to_arm.push((*v as i64, arm_idx));
                }
                TirPattern::Wildcard | TirPattern::Binding { .. } => {
                    // Wildcard/binding is the default case
                    if default_arm.is_some() {
                        // Multiple defaults - shouldn't happen, but bail out
                        return None;
                    }
                    default_arm = Some(arm_idx);
                }
                _ => {
                    // Non-integer pattern, can't use br_table
                    return None;
                }
            }
        }

        // Need at least MIN_CASES integer literals
        if value_to_arm.len() < BR_TABLE_MIN_CASES {
            return None;
        }

        // Calculate range
        let min_value = value_to_arm.iter().map(|(v, _)| *v).min().unwrap();
        let max_value = value_to_arm.iter().map(|(v, _)| *v).max().unwrap();
        let range = max_value - min_value + 1;

        // Check range isn't too large
        if range > BR_TABLE_MAX_RANGE {
            return None;
        }

        // Check density threshold
        let density = value_to_arm.len() as f64 / range as f64;
        if density < BR_TABLE_DENSITY_THRESHOLD {
            return None;
        }

        Some(BrTableAnalysis {
            min_value,
            max_value,
            value_to_arm,
            default_arm,
            is_i64,
        })
    }

    /// Generate match expression using `br_table` for O(1) dispatch
    ///
    /// Structure:
    /// ```text
    /// block $done (result T)
    ///   block $arm_0
    ///     block $arm_1
    ///       ...
    ///       block $default
    ///         local.get $scrutinee
    ///         i32.const <min_value>
    ///         i32.sub
    ///         br_table $arm_0 $arm_1 ... $default
    ///       end ;; $default
    ///       <default body>
    ///       br $done
    ///     end ;; $arm_1
    ///     <arm_1 body>
    ///     br $done
    ///   end ;; $arm_0
    ///   <arm_0 body>
    ///   ;; falls through to $done
    /// end ;; $done
    /// ```
    #[allow(clippy::too_many_arguments)]
    fn generate_match_br_table(
        &self,
        func: &mut Function,
        scrutinee_local: u32,
        arms: &[TirMatchArm],
        analysis: BrTableAnalysis,
        result_valtype: ValType,
        _result_type_id: TypeId,
        type_table: &TypeTable,
        ctx: &mut FunctionContext,
        builder: &CoreModuleBuilder,
    ) {
        let range = (analysis.max_value - analysis.min_value + 1) as usize;
        let num_arms = arms.len();

        // Create a lookup table: for each value in range, which arm to jump to
        // Default arm index is used for gaps
        let default_arm_idx = analysis.default_arm.unwrap_or(num_arms); // num_arms means unreachable

        let mut value_to_arm_map: Vec<usize> = vec![default_arm_idx; range];
        for (value, arm_idx) in &analysis.value_to_arm {
            let offset = (*value - analysis.min_value) as usize;
            value_to_arm_map[offset] = *arm_idx;
        }

        // Block structure:
        // - Block 0 ($done): outermost, result type T
        // - Block 1..num_arms ($arm_N): one per arm, for arm num_arms-1 down to 0
        // - Block num_arms+1 ($dispatch): innermost, contains br_table
        //
        // br_table targets are relative to the dispatch block:
        // - To reach $arm_i from dispatch block, we need depth = num_arms - i

        // Open $done block with result type
        func.instruction(&Instruction::Block(BlockType::Result(result_valtype)));

        // Open blocks for each arm (in reverse order, so arm 0's block is outermost)
        for _ in 0..num_arms {
            func.instruction(&Instruction::Block(BlockType::Empty));
        }

        // Open dispatch block
        func.instruction(&Instruction::Block(BlockType::Empty));

        // Load scrutinee and subtract min to get table index
        func.instruction(&Instruction::LocalGet(scrutinee_local));

        if analysis.min_value != 0 {
            if analysis.is_i64 {
                func.instruction(&Instruction::I64Const(analysis.min_value));
                func.instruction(&Instruction::I64Sub);
                // Convert to i32 for br_table index
                func.instruction(&Instruction::I32WrapI64);
            } else {
                func.instruction(&Instruction::I32Const(analysis.min_value as i32));
                func.instruction(&Instruction::I32Sub);
            }
        } else if analysis.is_i64 {
            // Just convert to i32
            func.instruction(&Instruction::I32WrapI64);
        }

        // Build br_table targets using the helper function
        let (targets, default_target) =
            Self::build_br_table_targets(&value_to_arm_map, num_arms, analysis.default_arm);

        func.instruction(&Instruction::BrTable(targets.into(), default_target));

        // End dispatch block
        func.instruction(&Instruction::End);

        // If no default arm, emit unreachable here (for out-of-range values)
        if analysis.default_arm.is_none() {
            func.instruction(&Instruction::Unreachable);
        }

        // Generate arm bodies in reverse order (arm[num_arms-1] first, arm[0] last)
        for arm_idx in (0..num_arms).rev() {
            // End the arm's block first
            func.instruction(&Instruction::End);

            // Generate this arm's body
            let arm = &arms[arm_idx];

            if let TirPattern::Binding { local_index, .. } = &arm.pattern {
                let adjusted_index = *local_index + ctx.local_index_offset;
                func.instruction(&Instruction::LocalGet(scrutinee_local));
                func.instruction(&Instruction::LocalSet(adjusted_index));
            }

            // Generate body expression
            self.generate_expr(func, &arm.body, type_table, ctx, builder);

            // Branch to $done (skip remaining arms)
            // From here, depth to $done is arm_idx + 1 (arm_idx blocks above us, plus $done)
            if arm_idx > 0 {
                func.instruction(&Instruction::Br((arm_idx) as u32));
            }
            // arm_idx == 0: falls through to $done naturally
        }

        // End $done block
        func.instruction(&Instruction::End);
    }

    /// Generate code for match expression: `match expr { pattern => body, ... }`
    ///
    /// Uses `br_table` optimization for dense integer matches (O(1) dispatch).
    /// Falls back to nested if-else chain for other patterns.
    ///
    /// `br_table` is used when:
    /// - All patterns are integer literals (plus optional wildcard/default)
    /// - At least 4 cases (`BR_TABLE_MIN_CASES`)
    /// - Density >= 40% (`BR_TABLE_DENSITY_THRESHOLD`)
    /// - Range <= 1024 (`BR_TABLE_MAX_RANGE`)
    ///
    /// Structure (as nested if-else, fallback):
    /// ```text
    /// block $match (result T)
    ///   ;; evaluate scrutinee once and store in local
    ///   local.set $scrutinee
    ///   ;; arm 0
    ///   local.get $scrutinee
    ///   <check pattern 0>
    ///   if (result T)
    ///     <bind pattern 0>
    ///     <body 0>
    ///   else
    ///     ;; arm 1
    ///     local.get $scrutinee
    ///     <check pattern 1>
    ///     if (result T)
    ///       <bind pattern 1>
    ///       <body 1>
    ///     else
    ///       ;; ... more arms
    ///       unreachable  ;; if no patterns match (non-exhaustive)
    ///     end
    ///   end
    /// end
    /// ```
    #[allow(clippy::too_many_arguments)]
    fn generate_match_expr(
        &self,
        func: &mut Function,
        scrutinee: &TirExpr,
        arms: &[TirMatchArm],
        result_type_id: TypeId,
        type_table: &TypeTable,
        ctx: &mut FunctionContext,
        builder: &CoreModuleBuilder,
    ) {
        // Evaluate scrutinee and store in a local
        self.generate_expr(func, scrutinee, type_table, ctx, builder);

        let scrutinee_valtype = self.type_id_to_valtype(type_table, scrutinee.type_id);
        let type_key = format!("match_{:?}", scrutinee.type_id);
        let local_name = ctx.next_match_scrutinee_local_name(&type_key);
        let scrutinee_local = ctx.alloc_local(&local_name, scrutinee_valtype);
        func.instruction(&Instruction::LocalSet(scrutinee_local));

        let result_valtype = self.type_id_to_valtype(type_table, result_type_id);

        // Try br_table optimization for integer matches
        let scrutinee_type = type_table.get(scrutinee.type_id);
        if let Some(analysis) = Self::analyze_for_br_table(arms, scrutinee_type) {
            self.generate_match_br_table(
                func,
                scrutinee_local,
                arms,
                analysis,
                result_valtype,
                result_type_id,
                type_table,
                ctx,
                builder,
            );
            return;
        }

        // Fall back to nested if-else chain for other patterns
        self.generate_match_arms(
            func,
            scrutinee_local,
            scrutinee.type_id,
            arms,
            0,
            result_valtype,
            result_type_id,
            type_table,
            ctx,
            builder,
        );
    }

    /// Generate match arms as nested if-else chain
    #[allow(clippy::too_many_arguments)]
    fn generate_match_arms(
        &self,
        func: &mut Function,
        scrutinee_local: u32,
        scrutinee_type_id: TypeId,
        arms: &[TirMatchArm],
        arm_index: usize,
        result_valtype: ValType,
        result_type_id: TypeId,
        type_table: &TypeTable,
        ctx: &mut FunctionContext,
        builder: &CoreModuleBuilder,
    ) {
        if arm_index >= arms.len() {
            // No more arms - unreachable (non-exhaustive match)
            func.instruction(&Instruction::Unreachable);
            return;
        }

        let arm = &arms[arm_index];
        let pattern = &arm.pattern;
        let scrutinee_type = type_table.get(scrutinee_type_id).clone();

        // Check if this is a wildcard or binding pattern (always matches)
        let is_irrefutable = matches!(pattern, TirPattern::Wildcard | TirPattern::Binding { .. });

        if is_irrefutable {
            // Irrefutable pattern - just bind and generate body directly
            self.generate_match_pattern_binding(
                func,
                scrutinee_local,
                scrutinee_type_id,
                pattern,
                type_table,
                ctx,
                builder,
            );
            self.generate_expr(func, &arm.body, type_table, ctx, builder);
        } else {
            // Refutable pattern - generate condition check
            // Get scrutinee value on stack
            func.instruction(&Instruction::LocalGet(scrutinee_local));

            // Generate pattern match check (leaves bool on stack)
            let condition_generated =
                self.generate_match_pattern_check(func, &scrutinee_type, pattern, type_table, ctx);

            if condition_generated {
                // Open if block with result type
                func.instruction(&Instruction::If(wasm_encoder::BlockType::Result(
                    result_valtype,
                )));
                if let Some((_, extra, _, _, _)) = ctx.loop_info.last_mut() {
                    *extra += 1;
                }

                // Then: pattern matches - bind and generate body
                self.generate_match_pattern_binding(
                    func,
                    scrutinee_local,
                    scrutinee_type_id,
                    pattern,
                    type_table,
                    ctx,
                    builder,
                );
                self.generate_expr(func, &arm.body, type_table, ctx, builder);

                // Else: try next arm
                func.instruction(&Instruction::Else);
                self.generate_match_arms(
                    func,
                    scrutinee_local,
                    scrutinee_type_id,
                    arms,
                    arm_index + 1,
                    result_valtype,
                    result_type_id,
                    type_table,
                    ctx,
                    builder,
                );

                func.instruction(&Instruction::End);
                if let Some((_, extra, _, _, _)) = ctx.loop_info.last_mut() {
                    *extra -= 1;
                }
            } else {
                // Pattern check not generated (unsupported pattern) - skip to next arm
                func.instruction(&Instruction::Drop);
                self.generate_match_arms(
                    func,
                    scrutinee_local,
                    scrutinee_type_id,
                    arms,
                    arm_index + 1,
                    result_valtype,
                    result_type_id,
                    type_table,
                    ctx,
                    builder,
                );
            }
        }
    }

    /// Generate code to check if a pattern matches (leaves bool on stack)
    /// Returns true if condition was generated, false if pattern is unsupported
    fn generate_match_pattern_check(
        &self,
        func: &mut Function,
        scrutinee_type: &ResolvedType,
        pattern: &TirPattern,
        _type_table: &TypeTable,
        _ctx: &mut FunctionContext,
    ) -> bool {
        match (scrutinee_type, pattern) {
            // Wildcard always matches
            (_, TirPattern::Wildcard) => {
                func.instruction(&Instruction::Drop);
                func.instruction(&Instruction::I32Const(1));
                true
            }

            // Binding always matches
            (_, TirPattern::Binding { .. }) => {
                func.instruction(&Instruction::Drop);
                func.instruction(&Instruction::I32Const(1));
                true
            }

            // Literal patterns
            (_, TirPattern::Literal(lit)) => {
                match lit {
                    TirLiteralPattern::I128(value) => {
                        // Generate comparison based on scrutinee type
                        match scrutinee_type {
                            ResolvedType::Primitive(PrimitiveType::I64) => {
                                func.instruction(&Instruction::I64Const(*value as i64));
                                func.instruction(&Instruction::I64Eq);
                            }
                            ResolvedType::Primitive(PrimitiveType::I128) => {
                                // i128 patterns should be lowered to Eq comparisons
                                panic!("i128 literal patterns should be lowered before codegen");
                            }
                            // I8, I16, I32 all use i32 at runtime
                            _ => {
                                func.instruction(&Instruction::I32Const(*value as i32));
                                func.instruction(&Instruction::I32Eq);
                            }
                        }
                    }
                    TirLiteralPattern::U128(value) => {
                        // Generate comparison based on scrutinee type
                        match scrutinee_type {
                            ResolvedType::Primitive(PrimitiveType::U64) => {
                                func.instruction(&Instruction::I64Const(*value as i64));
                                func.instruction(&Instruction::I64Eq);
                            }
                            ResolvedType::Primitive(PrimitiveType::U128) => {
                                // u128 patterns should be lowered to Eq comparisons
                                panic!("u128 literal patterns should be lowered before codegen");
                            }
                            // U8, U16, U32 all use i32 at runtime
                            _ => {
                                func.instruction(&Instruction::I32Const(*value as i32));
                                func.instruction(&Instruction::I32Eq);
                            }
                        }
                    }
                    TirLiteralPattern::Bool(value) => {
                        func.instruction(&Instruction::I32Const(i32::from(*value)));
                        func.instruction(&Instruction::I32Eq);
                    }
                    TirLiteralPattern::Char(value) => {
                        func.instruction(&Instruction::I32Const(*value as i32));
                        func.instruction(&Instruction::I32Eq);
                    }
                    TirLiteralPattern::Null => {
                        func.instruction(&Instruction::RefIsNull);
                    }
                    TirLiteralPattern::String(_value) => {
                        // String comparison requires calling string equality function
                        // For now, just return false (unsupported)
                        func.instruction(&Instruction::Drop);
                        func.instruction(&Instruction::I32Const(0));
                    }
                }
                true
            }

            // Option<T> with Some pattern - check for non-null
            (ResolvedType::Option(_), TirPattern::Variant { variant_name, .. })
                if variant_name == "Some" =>
            {
                func.instruction(&Instruction::RefIsNull);
                func.instruction(&Instruction::I32Eqz); // NOT null = Some
                true
            }

            // Option<T> with None pattern - check for null
            (ResolvedType::Option(_), TirPattern::Variant { variant_name, .. })
                if variant_name == "None" =>
            {
                func.instruction(&Instruction::RefIsNull); // null = None
                true
            }

            // Result<T, E> with Ok pattern - check if discriminant is 0
            (
                ResolvedType::Result { ok, err },
                TirPattern::Variant {
                    variant_name: case_name,
                    ..
                },
            ) => {
                // Build mangled name for Result<ok, err>
                let ok_name = _type_table.mangle_type_name(*ok);
                let err_name = _type_table.mangle_type_name(*err);
                let mangled_name = mangle_result_type(&ok_name, &err_name);

                let variant_types = &self.variant_types;
                let variant_info = variant_types.get(&mangled_name).unwrap_or_else(|| {
                    panic!("Result type not registered: {mangled_name}");
                });

                // Result has cases: Ok (0), Err (1)
                let case_index = usize::from(case_name != "Ok");
                let case_info = &variant_info.cases[case_index];
                let case_type_idx = case_info.type_idx;

                // Use ref.test to check if the value is of the expected case type
                func.instruction(&Instruction::RefTestNonNull(HeapType::Concrete(
                    case_type_idx,
                )));
                true
            }

            // Non-generic variant patterns
            (
                ResolvedType::Variant { name, .. },
                TirPattern::Variant {
                    variant_name: case_name,
                    ..
                },
            ) => {
                let variant_types = &self.variant_types;
                let variant_info = variant_types.get(name).unwrap_or_else(|| {
                    panic!("Variant type not registered: {name}");
                });

                let (case_index, case_info) = variant_info
                    .cases
                    .iter()
                    .enumerate()
                    .find(|(_, info)| info.name == *case_name)
                    .map(|(i, info)| (i, info.clone()))
                    .unwrap_or_else(|| panic!("Unknown case {case_name} for variant {name}"));
                let case_type_idx = case_info.type_idx;
                let base_type_idx = variant_info.base_type_idx;
                let is_unit_variant = case_info.payload_type.is_none();

                if is_unit_variant {
                    // Read discriminator and compare with case index
                    func.instruction(&Instruction::StructGet {
                        struct_type_index: base_type_idx,
                        field_index: 0,
                    });
                    func.instruction(&Instruction::I32Const(case_index as i32));
                    func.instruction(&Instruction::I32Eq);
                } else {
                    // Use ref.test to check if the value is of the expected case type
                    func.instruction(&Instruction::RefTestNonNull(HeapType::Concrete(
                        case_type_idx,
                    )));
                }
                true
            }

            // Generic instance variant patterns (Result<T,E>, Maybe<T>, etc.)
            (
                ResolvedType::GenericInstance {
                    name, type_args, ..
                },
                TirPattern::Variant {
                    variant_name: case_name,
                    ..
                },
            ) => {
                // Build mangled name including type arguments
                let type_arg_names: Vec<String> = type_args
                    .iter()
                    .map(|t| _type_table.mangle_type_name(*t))
                    .collect();
                let mangled_name = mangle_generic_name(name, &type_arg_names);

                let variant_types = &self.variant_types;
                let variant_info = variant_types.get(&mangled_name).unwrap_or_else(|| {
                    panic!("Variant type not registered: {mangled_name}");
                });

                let (case_index, case_info) = variant_info
                    .cases
                    .iter()
                    .enumerate()
                    .find(|(_, info)| info.name == *case_name)
                    .map(|(i, info)| (i, info.clone()))
                    .unwrap_or_else(|| {
                        panic!("Unknown case {case_name} for variant {mangled_name}")
                    });
                let case_type_idx = case_info.type_idx;
                let base_type_idx = variant_info.base_type_idx;
                let is_unit_variant = case_info.payload_type.is_none();

                if is_unit_variant {
                    // Read discriminator and compare with case index
                    func.instruction(&Instruction::StructGet {
                        struct_type_index: base_type_idx,
                        field_index: 0,
                    });
                    func.instruction(&Instruction::I32Const(case_index as i32));
                    func.instruction(&Instruction::I32Eq);
                } else {
                    // Use ref.test to check if the value is of the expected case type
                    func.instruction(&Instruction::RefTestNonNull(HeapType::Concrete(
                        case_type_idx,
                    )));
                }
                true
            }

            _ => {
                // Unsupported pattern
                false
            }
        }
    }

    /// Generate code to bind pattern variables after a successful match
    #[allow(clippy::too_many_arguments)]
    fn generate_match_pattern_binding(
        &self,
        func: &mut Function,
        scrutinee_local: u32,
        scrutinee_type_id: TypeId,
        pattern: &TirPattern,
        type_table: &TypeTable,
        ctx: &mut FunctionContext,
        builder: &CoreModuleBuilder,
    ) {
        let scrutinee_type = type_table.get(scrutinee_type_id).clone();

        match (scrutinee_type, pattern) {
            // Wildcard - no binding needed
            (_, TirPattern::Wildcard) => {}

            // Simple binding - bind scrutinee value
            (_, TirPattern::Binding { local_index, .. }) => {
                func.instruction(&Instruction::LocalGet(scrutinee_local));
                func.instruction(&Instruction::LocalSet(*local_index));
            }

            // Literal - no binding needed
            (_, TirPattern::Literal(_)) => {}

            // Option<T> with Some(x) pattern - extract inner value
            (
                ResolvedType::Option(inner_type),
                TirPattern::Variant {
                    variant_name,
                    bindings,
                    payload_type,
                    ..
                },
            ) if variant_name == "Some" => {
                if let Some(binding) = bindings.first() {
                    // Get the inner value (non-null reference)
                    func.instruction(&Instruction::LocalGet(scrutinee_local));
                    func.instruction(&Instruction::RefAsNonNull);

                    // Unbox primitive if needed
                    if let TirPattern::Binding { local_index, .. } = binding {
                        let is_address_taken = ctx.address_taken_locals.contains(local_index);
                        if !is_address_taken
                            && let ResolvedType::Primitive(prim) = type_table.get(inner_type)
                        {
                            let val_type = primitive_to_valtype(prim);
                            if let Some(box_type_idx) = self.get_box_type_idx(val_type) {
                                func.instruction(&Instruction::StructGet {
                                    struct_type_index: box_type_idx,
                                    field_index: 0,
                                });
                            }
                        }
                    }

                    self.generate_let_pattern_binding(
                        func,
                        binding,
                        *payload_type,
                        type_table,
                        ctx,
                        builder,
                    );
                }
            }

            // Option<T> with None pattern - no binding needed
            (ResolvedType::Option(_), TirPattern::Variant { variant_name, .. })
                if variant_name == "None" => {}

            // Result<T, E> with Ok(x) or Err(e) pattern - extract inner value
            (
                ResolvedType::Result { ok, err },
                TirPattern::Variant {
                    variant_name: case_name,
                    bindings,
                    payload_type,
                    ..
                },
            ) => {
                if let Some(binding) = bindings.first() {
                    // Build mangled name for Result<ok, err>
                    let ok_name = type_table.mangle_type_name(ok);
                    let err_name = type_table.mangle_type_name(err);
                    let mangled_name = mangle_result_type(&ok_name, &err_name);

                    let variant_types = &self.variant_types;
                    let variant_info = variant_types.get(&mangled_name).unwrap();
                    let case_index = usize::from(case_name != "Ok");
                    let case_info = variant_info.cases[case_index].clone();
                    let case_type_idx = case_info.type_idx;

                    // Get payload (field 1)
                    func.instruction(&Instruction::LocalGet(scrutinee_local));
                    func.instruction(&Instruction::RefCastNonNull(HeapType::Concrete(
                        case_type_idx,
                    )));
                    func.instruction(&Instruction::StructGet {
                        struct_type_index: case_type_idx,
                        field_index: 1,
                    });

                    self.generate_let_pattern_binding(
                        func,
                        binding,
                        *payload_type,
                        type_table,
                        ctx,
                        builder,
                    );
                }
            }

            // Non-generic variant patterns
            (
                ResolvedType::Variant { name, .. },
                TirPattern::Variant {
                    variant_name: case_name,
                    bindings,
                    payload_type,
                    ..
                },
            ) => {
                if let Some(binding) = bindings.first() {
                    let variant_types = &self.variant_types;
                    let variant_info = variant_types.get(&name).unwrap();
                    let case_info = variant_info
                        .cases
                        .iter()
                        .find(|info| info.name == *case_name)
                        .unwrap()
                        .clone();
                    let case_type_idx = case_info.type_idx;

                    // Get payload (field 1)
                    func.instruction(&Instruction::LocalGet(scrutinee_local));
                    func.instruction(&Instruction::RefCastNonNull(HeapType::Concrete(
                        case_type_idx,
                    )));
                    func.instruction(&Instruction::StructGet {
                        struct_type_index: case_type_idx,
                        field_index: 1,
                    });

                    self.generate_let_pattern_binding(
                        func,
                        binding,
                        *payload_type,
                        type_table,
                        ctx,
                        builder,
                    );
                }
            }

            // Generic instance variant patterns (Result<T,E>, Maybe<T>, etc.)
            (
                ResolvedType::GenericInstance {
                    name, type_args, ..
                },
                TirPattern::Variant {
                    variant_name: case_name,
                    bindings,
                    payload_type,
                    ..
                },
            ) => {
                if let Some(binding) = bindings.first() {
                    // Build mangled name including type arguments
                    let type_arg_names: Vec<String> = type_args
                        .iter()
                        .map(|t| type_table.mangle_type_name(*t))
                        .collect();
                    let mangled_name = mangle_generic_name(&name, &type_arg_names);

                    let variant_types = &self.variant_types;
                    let variant_info = variant_types.get(&mangled_name).unwrap();
                    let case_info = variant_info
                        .cases
                        .iter()
                        .find(|info| info.name == *case_name)
                        .unwrap()
                        .clone();
                    let case_type_idx = case_info.type_idx;

                    // Get payload (field 1)
                    func.instruction(&Instruction::LocalGet(scrutinee_local));
                    func.instruction(&Instruction::RefCastNonNull(HeapType::Concrete(
                        case_type_idx,
                    )));
                    func.instruction(&Instruction::StructGet {
                        struct_type_index: case_type_idx,
                        field_index: 1,
                    });

                    self.generate_let_pattern_binding(
                        func,
                        binding,
                        *payload_type,
                        type_table,
                        ctx,
                        builder,
                    );
                }
            }

            // Tuple pattern
            (ResolvedType::Tuple(elem_types), TirPattern::Tuple(patterns)) => {
                let tuple_type_idx = self
                    .get_tuple_type_idx(&elem_types)
                    .expect("Tuple type not registered");
                for (i, (pat, &elem_type)) in patterns.iter().zip(elem_types.iter()).enumerate() {
                    if matches!(pat, TirPattern::Wildcard) {
                        continue;
                    }
                    func.instruction(&Instruction::LocalGet(scrutinee_local));
                    func.instruction(&Instruction::StructGet {
                        struct_type_index: tuple_type_idx,
                        field_index: i as u32,
                    });
                    self.generate_let_pattern_binding(
                        func, pat, elem_type, type_table, ctx, builder,
                    );
                }
            }

            _ => {
                // Unsupported pattern - do nothing
            }
        }
    }

    /// Generate a Wasm function from TIR function
    ///
    /// Returns the generated function and any branch hints collected during generation.
    fn generate_function(
        &self,
        tir_func: &TirFunction,
        type_table: &TypeTable,
        builder: &CoreModuleBuilder,
        module_path: &[String],
    ) -> (Function, Vec<(u32, bool)>) {
        // Create function context - TIR already has local count and types
        let mut func_ctx =
            FunctionContext::with_module_path(tir_func.params.len() as u32, module_path.to_vec());

        // Copy address-taken locals from TIR
        func_ctx.address_taken_locals = tir_func.address_taken_locals.clone();

        // Set return type
        if tir_func.return_type != TypeTable::UNIT {
            func_ctx.set_return_type(self.type_id_to_valtype(type_table, tir_func.return_type));
        }

        // Add parameters to context
        for param in &tir_func.params {
            let param_type = self.type_id_to_valtype(type_table, param.type_id);
            func_ctx.add_param(&param.name, param_type);
        }

        // Pre-allocate locals from TIR (skip params which are already added)
        for (i, &local_type_id) in tir_func.local_types.iter().enumerate() {
            let local_idx = i as u32;
            // Skip if it's a param (already added)
            if local_idx < tir_func.params.len() as u32 {
                continue;
            }

            // For address-taken primitive locals, use box type instead
            let local_type = if func_ctx.address_taken_locals.contains(&local_idx) {
                if let ResolvedType::Primitive(prim) = type_table.get(local_type_id) {
                    let val_type = primitive_to_valtype(prim);
                    if let Some(box_type_idx) = self.get_box_type_idx(val_type) {
                        // Track the box type for this local
                        func_ctx.local_box_types.insert(local_idx, box_type_idx);
                        // Use nullable reference for locals (they start uninitialized)
                        ValType::Ref(RefType {
                            nullable: true,
                            heap_type: HeapType::Concrete(box_type_idx),
                        })
                    } else {
                        self.type_id_to_valtype(type_table, local_type_id)
                    }
                } else {
                    // Non-primitive address-taken locals don't need boxing
                    self.type_id_to_valtype(type_table, local_type_id)
                }
            } else {
                self.type_id_to_valtype(type_table, local_type_id)
            };

            let local_name = format!("_local_{local_idx}");
            func_ctx.alloc_local(&local_name, local_type);
        }

        self.allocate_precomputed_scratch_locals(tir_func, type_table, &mut func_ctx);
        func_ctx.reset_let_pattern_counter();
        func_ctx.reset_match_scrutinee_counter();

        // Generate the function code
        let mut wasm_func = Function::new(func_ctx.get_local_decls());

        // Generate body
        if let Some(body) = &tir_func.body {
            self.generate_block(&mut wasm_func, body, type_table, &mut func_ctx, builder);
        }

        // Add implicit return handling
        if tir_func.return_type == TypeTable::UNIT {
            // Unit return - no value needed
        } else {
            // Non-unit return: add unreachable in case all paths return early
            // (e.g., if/else where both branches have return statements).
            // This satisfies Wasm's type checker which requires a value on stack at function end.
            wasm_func.instruction(&Instruction::Unreachable);
        }
        wasm_func.instruction(&Instruction::End);

        // Return function and collected branch hints
        let branch_hints = func_ctx.branch_hints;
        (wasm_func, branch_hints)
    }

    /// Generate the 'run' function for TIR with task.return wrapper
    ///
    /// This is a special case of function generation for the WASI CLI entry point.
    /// It generates the function body and appends task.return before End.
    fn generate_run_function(
        &self,
        tir_func: &TirFunction,
        type_table: &TypeTable,
        builder: &CoreModuleBuilder,
    ) -> Function {
        // Create function context
        let mut func_ctx = FunctionContext::new(tir_func.params.len() as u32);

        // Set async export flags from CmExportInfo (computed by wasm_adapt phase)
        if let Some(cm_info) = &tir_func.cm_export_info {
            func_ctx.is_async_export = cm_info.is_async;
            func_ctx.has_http_handler_export = cm_info.is_http_handler;
        } else {
            // Fallback for functions without CmExportInfo (shouldn't happen for world exports)
            func_ctx.is_async_export = true;
            func_ctx.has_http_handler_export = self.project.has_http_handler_export;
        }

        // Copy address-taken locals from TIR
        func_ctx.address_taken_locals = tir_func.address_taken_locals.clone();

        // Add parameters to context
        for param in &tir_func.params {
            let param_type = self.type_id_to_valtype(type_table, param.type_id);
            func_ctx.add_param(&param.name, param_type);
        }

        // Pre-allocate locals from TIR (skip params which are already added)
        for (i, &local_type_id) in tir_func.local_types.iter().enumerate() {
            let local_idx = i as u32;
            // Skip if it's a param (already added)
            if local_idx < tir_func.params.len() as u32 {
                continue;
            }

            // For address-taken primitive locals, use box type instead
            let local_type = if func_ctx.address_taken_locals.contains(&local_idx) {
                if let ResolvedType::Primitive(prim) = type_table.get(local_type_id) {
                    let val_type = primitive_to_valtype(prim);
                    if let Some(box_type_idx) = self.get_box_type_idx(val_type) {
                        // Track the box type for this local
                        func_ctx.local_box_types.insert(local_idx, box_type_idx);
                        // Use nullable reference for locals (they start uninitialized)
                        ValType::Ref(RefType {
                            nullable: true,
                            heap_type: HeapType::Concrete(box_type_idx),
                        })
                    } else {
                        self.type_id_to_valtype(type_table, local_type_id)
                    }
                } else {
                    // Non-primitive address-taken locals don't need boxing
                    self.type_id_to_valtype(type_table, local_type_id)
                }
            } else {
                self.type_id_to_valtype(type_table, local_type_id)
            };

            let local_name = format!("_local_{local_idx}");
            func_ctx.alloc_local(&local_name, local_type);
        }

        self.allocate_precomputed_scratch_locals(tir_func, type_table, &mut func_ctx);

        // Pre-allocate scratch locals for Service world HTTP response creation
        if func_ctx.is_async_export && func_ctx.has_http_handler_export {
            func_ctx.alloc_local("_http_future", ValType::I64);
            func_ctx.alloc_local("_trailers_rx", ValType::I32);
            func_ctx.alloc_local("_trailers_tx", ValType::I32);
            func_ctx.alloc_local("_headers_handle", ValType::I32);
            func_ctx.alloc_local("_write_result", ValType::I32);
            func_ctx.alloc_local("_result_disc", ValType::I32);
            func_ctx.alloc_local("_response_handle", ValType::I32);
        }

        // Reset let-pattern counter so code generation uses the same indices as pre-allocation
        func_ctx.reset_let_pattern_counter();
        // Reset match-scrutinee counter so code generation uses the same indices as pre-allocation
        func_ctx.reset_match_scrutinee_counter();

        let mut wasm_func = Function::new(func_ctx.get_local_decls());

        // Generate body
        if let Some(body) = &tir_func.body {
            self.generate_block(&mut wasm_func, body, type_table, &mut func_ctx, builder);
        }

        // For async exports, ensure task-return is called even for fall-through paths
        // (functions without explicit return statements)
        if func_ctx.is_async_export {
            // For Service world: result<own<response>, error-code> with complex payloads
            // The full error-code variant flattens to: (i32, i32, i32, i64, i32, i32, i32, i32)
            // For Command world: result<_, _> needs just (i32)
            if func_ctx.has_http_handler_export {
                wasm_func.instruction(&Instruction::I32Const(1)); // Err discriminant
                wasm_func.instruction(&Instruction::I32Const(38)); // internal-error discriminant
                wasm_func.instruction(&Instruction::I32Const(1)); // option<string> = Some
                wasm_func.instruction(&Instruction::I64Const(0)); // string ptr
                wasm_func.instruction(&Instruction::I32Const(37)); // string len
                wasm_func.instruction(&Instruction::I32Const(0)); // padding
                wasm_func.instruction(&Instruction::I32Const(0)); // padding
                wasm_func.instruction(&Instruction::I32Const(0)); // padding
            } else {
                wasm_func.instruction(&Instruction::I32Const(0)); // Ok discriminant for result<_, _>
            }
            let task_return_idx = builder.func_idx("task-return");
            wasm_func.instruction(&Instruction::Call(task_return_idx));
        }
        wasm_func.instruction(&Instruction::End);

        wasm_func
    }

    /// Resolve a TIR function call target to its function index
    ///
    /// This handles:
    /// 1. Local functions (simple name lookup)
    /// 2. Builtin functions (`builtin::` namespace)
    /// 3. Core library functions (`core::` namespace)
    /// 4. Qualified names from imported modules
    fn resolve_call_target(
        &self,
        module_path: &[String],
        func_name: &str,
        current_module_path: &[String],
        builder: &CoreModuleBuilder,
    ) -> u32 {
        // Strategy 1: Try simple name lookup first (for local functions)
        if module_path.is_empty()
            && let Some(idx) = builder.try_func_idx(func_name)
        {
            return idx;
        }

        // Strategy 2: Check if it's a builtin function (module_path == ["core", "builtin"])
        if module_path == ["core", "builtin"]
            && let Some(builtin_info) = self.project.builtin_registry.get(func_name)
            && let Some(canonical_name) = &builtin_info.canonical_name
            && let Some(idx) = builder.try_func_idx(canonical_name)
        {
            return idx;
        }

        // Invariant: TirExprKind::Call should never have method names (containing "::")
        // Methods use TirExprKind::MethodCall instead.
        debug_assert!(
            !func_name.contains("::"),
            "TirExprKind::Call should not have method-style names: {func_name}"
        );

        // Strategy 3: Build mangled name and try lookup
        let mangled_name = if module_path.is_empty() {
            func_name.to_string()
        } else {
            FreeFunctionName::from_path_and_name(module_path, func_name).to_string()
        };

        if let Some(idx) = builder.try_func_idx(&mangled_name) {
            return idx;
        }

        // Strategy 4: Try current module path for local function calls
        // When module_path is empty, try the current function's module
        if module_path.is_empty() && !current_module_path.is_empty() {
            let current_mangled_name =
                FreeFunctionName::from_path_and_name(current_module_path, func_name).to_string();
            if let Some(idx) = builder.try_func_idx(&current_mangled_name) {
                return idx;
            }
        }

        // Strategy 5: Try core internal name format
        if module_path == ["core", "internal"] {
            let internal_name = build_core_internal_name(func_name).to_string();
            if let Some(idx) = builder.try_func_idx(&internal_name) {
                return idx;
            }
        }

        // Strategy 6: Try core/cli function format (for println, eprintln, etc.)
        if module_path == ["core", "cli"] {
            let cli_name = FreeFunctionName::from_strs(&["core", "cli"], func_name).to_string();
            if let Some(idx) = builder.try_func_idx(&cli_name) {
                return idx;
            }
        }

        // Strategy 7: Try WASI effect operation resolution
        // When module_path has a single element like ["Stdout"], try resolving as an effect operation
        if module_path.len() == 1 {
            let effect_qualified_name = format!("{}::{}", module_path[0], func_name);
            if let Some(wasi_local_name) =
                self.project.wasi_registry.resolve(&effect_qualified_name)
                && let Some(idx) = builder.try_func_idx(&wasi_local_name)
            {
                return idx;
            }
        }

        // If we get here, the function wasn't found
        let full_name = if module_path.is_empty() {
            func_name.to_string()
        } else {
            format!("{}::{}", module_path.join("::"), func_name)
        };
        panic!("unknown function: {full_name}");
    }

    /// Generate a CM effect call using the convention from `WasiRegistry`
    ///
    /// This method handles all the CM ABI details:
    /// - Outptr allocation for complex return types
    /// - Calling the WASI function
    /// - Result conversion (list to array, tuple struct creation, etc.)
    /// - Async operation handling (subtask storage)
    ///
    /// Returns true if the call was handled, false if it's not a known WASI function.
    #[allow(clippy::too_many_arguments)]
    fn generate_cm_effect_call(
        &self,
        func: &mut Function,
        ctx: &mut FunctionContext,
        builder: &CoreModuleBuilder,
        type_table: &TypeTable,
        effect_name: &str,
        op_name: &str,
        args: &[TirExpr],
    ) -> bool {
        let qualified_name = format!("{effect_name}::{op_name}");
        let Some(func_info) = self.project.wasi_registry.get_function(&qualified_name) else {
            return false;
        };

        let conv = &func_info.call_convention;
        let local_name = func_info.local_alias_name();

        // Generate arguments first
        self.generate_args(func, args, type_table, ctx, builder);

        // Handle async operations (need extra outptr argument)
        // For async functions, the outptr is always 2048 - we don't use outptr_alloc
        if conv.is_async {
            func.instruction(&Instruction::I32Const(2048)); // outptr for async result
        } else if let Some((size, align)) = conv.outptr_alloc {
            // Handle outptr allocation for complex return types (sync functions only)
            // Allocate outptr using realloc
            func.instruction(&Instruction::I32Const(0)); // old_ptr
            func.instruction(&Instruction::I32Const(0)); // old_size
            func.instruction(&Instruction::I32Const(align as i32)); // align
            func.instruction(&Instruction::I32Const(size as i32)); // new_size
            let realloc_idx = builder.func_idx("realloc");
            func.instruction(&Instruction::Call(realloc_idx));

            // Store outptr for later use
            let outptr_local = ctx.get_local("__cm_outptr").expect(
                "__cm_outptr should be pre-allocated for functions with CM complex returns",
            );
            func.instruction(&Instruction::LocalTee(outptr_local));
        }

        // Call the WASI function
        let func_idx = builder.func_idx(&local_name);
        func.instruction(&Instruction::Call(func_idx));

        // Handle async operation result (store subtask handle)
        if conv.is_async {
            let subtask_local = ctx
                .get_local("__subtask")
                .expect("__subtask should be pre-allocated for functions with async effects");
            func.instruction(&Instruction::LocalSet(subtask_local));
            return true;
        }

        // Handle result conversion
        if let Some(ref converter) = conv.result_converter {
            let outptr_local = ctx.get_local("__cm_outptr").expect(
                "__cm_outptr should be pre-allocated for functions with CM complex returns",
            );
            func.instruction(&Instruction::LocalGet(outptr_local));
            let conv_idx = builder.func_idx(converter);
            func.instruction(&Instruction::Call(conv_idx));
        } else if let Some(ref elements) = conv.tuple_return {
            // Create tuple struct from outptr values
            // Pattern: Load all values onto stack, then StructNew consumes them all
            let outptr_local = ctx
                .get_local("__cm_outptr")
                .expect("__cm_outptr should be pre-allocated for functions with tuple returns");

            // Convert CmPrimitiveType to TypeId for tuple type lookup
            let type_ids: Vec<TypeId> = elements
                .iter()
                .map(|p| match p {
                    CmPrimitiveType::I32 => TypeTable::I32,
                    CmPrimitiveType::I64 => TypeTable::I64,
                    CmPrimitiveType::U32 => TypeTable::U32,
                    CmPrimitiveType::U64 => TypeTable::U64,
                    CmPrimitiveType::F32 => TypeTable::F32,
                    CmPrimitiveType::F64 => TypeTable::F64,
                })
                .collect();

            // Load all values from outptr onto the stack
            let mut offset: u32 = 0;
            for prim in elements {
                // Align offset
                let align = prim.align();
                if !offset.is_multiple_of(align) {
                    offset += align - (offset % align);
                }

                // Load value from outptr
                func.instruction(&Instruction::LocalGet(outptr_local));
                match prim {
                    CmPrimitiveType::I32 | CmPrimitiveType::U32 => {
                        func.instruction(&Instruction::I32Load(wasm_encoder::MemArg {
                            offset: u64::from(offset),
                            align: 2,
                            memory_index: 0,
                        }));
                    }
                    CmPrimitiveType::I64 | CmPrimitiveType::U64 => {
                        func.instruction(&Instruction::I64Load(wasm_encoder::MemArg {
                            offset: u64::from(offset),
                            align: 3,
                            memory_index: 0,
                        }));
                    }
                    CmPrimitiveType::F32 => {
                        func.instruction(&Instruction::F32Load(wasm_encoder::MemArg {
                            offset: u64::from(offset),
                            align: 2,
                            memory_index: 0,
                        }));
                    }
                    CmPrimitiveType::F64 => {
                        func.instruction(&Instruction::F64Load(wasm_encoder::MemArg {
                            offset: u64::from(offset),
                            align: 3,
                            memory_index: 0,
                        }));
                    }
                }

                offset += prim.size();
            }

            // Create tuple struct - consumes all values on stack
            if let Some(type_idx) = self.get_tuple_type_idx(&type_ids) {
                func.instruction(&Instruction::StructNew(type_idx));
            } else {
                panic!("tuple type {type_ids:?} not registered for CM return conversion");
            }
        } else if conv.option_resource_return {
            // option<own<resource>> - box the i32 handle if Some
            let outptr_local = ctx.get_local("__cm_outptr").expect(
                "__cm_outptr should be pre-allocated for functions with option<resource> returns",
            );
            // Load the discriminant/handle value
            func.instruction(&Instruction::LocalGet(outptr_local));
            func.instruction(&Instruction::I32Load(wasm_encoder::MemArg {
                offset: 0,
                align: 2,
                memory_index: 0,
            }));
            // Box it as Option<i32>: 0 = None, non-zero = Some(value as i32 box)
            let box_i32_idx = builder.func_idx("core/internal/box_i32_for_option");
            func.instruction(&Instruction::Call(box_i32_idx));
        }

        true
    }

    /// Generate wait logic for pending effect subtasks
    ///
    /// This should be called at the end of functions that use Stdout/Stderr effects.
    /// It waits for the subtask started by write-via-stream to complete.
    fn generate_effect_wait(
        &self,
        func: &mut Function,
        ctx: &FunctionContext,
        builder: &CoreModuleBuilder,
    ) {
        // Check if we have a subtask to wait for
        let subtask_local = match ctx.get_local("__subtask") {
            Some(idx) => idx,
            None => return, // No pending subtask
        };

        let waitable_set_new_idx = builder.func_idx("waitable-set-new");
        let waitable_join_idx = builder.func_idx("waitable-join");
        let waitable_set_wait_idx = builder.func_idx("waitable-set-wait");
        let subtask_drop_idx = builder.func_idx("subtask-drop");
        let waitable_set_local = ctx.get_local("__waitable_set").unwrap_or(subtask_local + 1);

        // Check if subtask is pending
        // If (status & 1) == 0, the operation is still pending and we need to wait
        func.instruction(&Instruction::LocalGet(subtask_local));
        func.instruction(&Instruction::I32Const(1));
        func.instruction(&Instruction::I32And);
        func.instruction(&Instruction::I32Eqz);
        func.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));

        // Subtask is pending - need to wait for it
        // Create waitable-set
        func.instruction(&Instruction::Call(waitable_set_new_idx));
        func.instruction(&Instruction::LocalSet(waitable_set_local));

        // Join subtask to waitable-set
        func.instruction(&Instruction::LocalGet(waitable_set_local));
        func.instruction(&Instruction::LocalGet(subtask_local));
        func.instruction(&Instruction::Call(waitable_join_idx));

        // Wait for completion
        func.instruction(&Instruction::LocalGet(waitable_set_local));
        func.instruction(&Instruction::I32Const(2048)); // outptr
        func.instruction(&Instruction::Call(waitable_set_wait_idx));
        func.instruction(&Instruction::Drop); // drop wait result

        // Drop subtask
        func.instruction(&Instruction::LocalGet(subtask_local));
        func.instruction(&Instruction::Call(subtask_drop_idx));

        func.instruction(&Instruction::End); // end if
    }

    /// Generate code for a builtin function call.
    /// Handles both intrinsics (inline Wasm instructions) and builtins with canonical mappings.
    /// Panics if the builtin is unknown.
    #[allow(clippy::too_many_arguments)]
    fn generate_builtin_call(
        &self,
        builtin_name: &str,
        args: &[TirExpr],
        expr: &TirExpr,
        func: &mut Function,
        type_table: &TypeTable,
        ctx: &mut FunctionContext,
        builder: &CoreModuleBuilder,
    ) {
        match builtin_name {
            "builtin::likely" => {
                // Pass through the argument and set branch hint for the next branch
                self.generate_args(func, args, type_table, ctx, builder);
                ctx.set_branch_hint(true);
            }
            "builtin::unlikely" => {
                // Pass through the argument and set branch hint for the next branch
                self.generate_args(func, args, type_table, ctx, builder);
                ctx.set_branch_hint(false);
            }
            "builtin::unreachable" => {
                func.instruction(&Instruction::Unreachable);
            }
            "builtin::effect_wait" => {
                self.generate_effect_wait(func, ctx, builder);
            }
            "builtin::array_len" => {
                // Generate array argument and ensure it's non-null
                if let Some(arr_arg) = args.first() {
                    self.generate_expr(func, arr_arg, type_table, ctx, builder);
                    func.instruction(&Instruction::RefAsNonNull);
                }
                func.instruction(&Instruction::ArrayLen);
            }
            "builtin::array_get_u8" => {
                self.generate_args(func, args, type_table, ctx, builder);
                let u8_array_idx = self.get_array_type_index(TypeTable::U8);
                func.instruction(&Instruction::ArrayGetU(u8_array_idx));
            }
            "builtin::array_set_u8" => {
                self.generate_args(func, args, type_table, ctx, builder);
                let u8_array_idx = self.get_array_type_index(TypeTable::U8);
                func.instruction(&Instruction::ArraySet(u8_array_idx));
            }
            "builtin::memory_store8" => {
                self.generate_args(func, args, type_table, ctx, builder);
                func.instruction(&Instruction::I32Store8(MemArg {
                    offset: 0,
                    align: 0,
                    memory_index: 0,
                }));
            }
            "builtin::memory_load8_u" => {
                self.generate_args(func, args, type_table, ctx, builder);
                func.instruction(&Instruction::I32Load8U(MemArg {
                    offset: 0,
                    align: 0,
                    memory_index: 0,
                }));
            }
            "builtin::memory_load32" => {
                self.generate_args(func, args, type_table, ctx, builder);
                func.instruction(&Instruction::I32Load(MemArg {
                    offset: 0,
                    align: 2,
                    memory_index: 0,
                }));
            }
            "builtin::array_new" => {
                if let ResolvedType::BuiltinArray(element_type) = type_table.get(expr.type_id) {
                    let array_type_idx = self.get_array_type_index(*element_type);
                    self.generate_args(func, args, type_table, ctx, builder);
                    func.instruction(&Instruction::ArrayNewDefault(array_type_idx));
                } else {
                    panic!("array_new return type must be builtin::array<T>");
                }
            }
            "builtin::array_get" => {
                if let Some(arr_arg) = args.first() {
                    if let ResolvedType::BuiltinArray(element_type) =
                        type_table.get(arr_arg.type_id)
                    {
                        let array_type_idx = self.get_array_type_index(*element_type);
                        // Generate array argument and ensure it's non-null
                        // (struct field access returns nullable refs)
                        self.generate_expr(func, arr_arg, type_table, ctx, builder);
                        func.instruction(&Instruction::RefAsNonNull);
                        // Generate remaining arguments (index)
                        self.generate_args(func, &args[1..], type_table, ctx, builder);
                        // For packed types (i8/u8/i16/u16), use ArrayGetS/ArrayGetU
                        // Use matches! on the actual type, not TypeId comparison,
                        // because TypeIds may differ across modules
                        let elem_resolved = type_table.get(*element_type);
                        if matches!(
                            elem_resolved,
                            ResolvedType::Primitive(PrimitiveType::U8 | PrimitiveType::U16)
                        ) {
                            func.instruction(&Instruction::ArrayGetU(array_type_idx));
                        } else if matches!(
                            elem_resolved,
                            ResolvedType::Primitive(PrimitiveType::I8 | PrimitiveType::I16)
                        ) {
                            func.instruction(&Instruction::ArrayGetS(array_type_idx));
                        } else {
                            func.instruction(&Instruction::ArrayGet(array_type_idx));
                        }
                        // For reference element types, array.get returns nullable ref but
                        // the expected return type is non-null, so add ref.as_non_null
                        if self.type_is_reference(*element_type, type_table) {
                            func.instruction(&Instruction::RefAsNonNull);
                        }
                    } else {
                        panic!("array_get first argument must be builtin::array<T>");
                    }
                }
            }
            "builtin::array_set" => {
                if let Some(arr_arg) = args.first() {
                    if let ResolvedType::BuiltinArray(element_type) =
                        type_table.get(arr_arg.type_id)
                    {
                        let array_type_idx = self.get_array_type_index(*element_type);
                        // Generate array argument and ensure it's non-null
                        // (struct field access returns nullable refs)
                        self.generate_expr(func, arr_arg, type_table, ctx, builder);
                        func.instruction(&Instruction::RefAsNonNull);
                        // Generate remaining arguments (index, value)
                        self.generate_args(func, &args[1..], type_table, ctx, builder);
                        func.instruction(&Instruction::ArraySet(array_type_idx));
                    } else {
                        panic!("array_set first argument must be builtin::array<T>");
                    }
                }
            }
            "builtin::array_copy" => {
                // array.copy: (dst_arr, dst_offset, src_arr, src_offset, len)
                // Both arrays need to be non-null
                if let Some(dst_arg) = args.first() {
                    if let ResolvedType::BuiltinArray(element_type) =
                        type_table.get(dst_arg.type_id)
                    {
                        let array_type_idx = self.get_array_type_index(*element_type);
                        // Generate dst array and ensure non-null
                        self.generate_expr(func, &args[0], type_table, ctx, builder);
                        func.instruction(&Instruction::RefAsNonNull);
                        // dst_offset
                        self.generate_expr(func, &args[1], type_table, ctx, builder);
                        // Generate src array and ensure non-null
                        self.generate_expr(func, &args[2], type_table, ctx, builder);
                        func.instruction(&Instruction::RefAsNonNull);
                        // src_offset
                        self.generate_expr(func, &args[3], type_table, ctx, builder);
                        // len
                        self.generate_expr(func, &args[4], type_table, ctx, builder);
                        func.instruction(&Instruction::ArrayCopy {
                            array_type_index_dst: array_type_idx,
                            array_type_index_src: array_type_idx,
                        });
                    } else {
                        panic!("array_copy first argument must be builtin::array<T>");
                    }
                }
            }
            "builtin::array_fill" => {
                // array.fill $t : [(ref null $t) i32 t i32] -> []
                // args: (arr, offset, value, len)
                if let Some(arr_arg) = args.first() {
                    if let ResolvedType::BuiltinArray(element_type) =
                        type_table.get(arr_arg.type_id)
                    {
                        let array_type_idx = self.get_array_type_index(*element_type);
                        self.generate_args(func, args, type_table, ctx, builder);
                        func.instruction(&Instruction::ArrayFill(array_type_idx));
                    } else {
                        panic!("array_fill first argument must be builtin::array<T>");
                    }
                }
            }
            "builtin::i32_and" => {
                self.generate_args(func, args, type_table, ctx, builder);
                func.instruction(&Instruction::I32And);
            }
            "builtin::i32_eqz" => {
                self.generate_args(func, args, type_table, ctx, builder);
                func.instruction(&Instruction::I32Eqz);
            }
            // Wide Arithmetic builtins - map directly to Wasm wide-arithmetic instructions
            // These return multi-value [i64, i64] which is wrapped in a tuple struct
            // unless skip_tuple_wrap is set (for tuple elision optimization)
            "builtin::i64_add128" => {
                self.generate_args(func, args, type_table, ctx, builder);
                func.instruction(&Instruction::I64Add128);
                if !ctx.skip_tuple_wrap {
                    let tuple_type_idx =
                        self.get_struct_or_tuple_type_idx(expr.type_id, type_table);
                    func.instruction(&Instruction::StructNew(tuple_type_idx));
                }
            }
            "builtin::i64_sub128" => {
                self.generate_args(func, args, type_table, ctx, builder);
                func.instruction(&Instruction::I64Sub128);
                if !ctx.skip_tuple_wrap {
                    let tuple_type_idx =
                        self.get_struct_or_tuple_type_idx(expr.type_id, type_table);
                    func.instruction(&Instruction::StructNew(tuple_type_idx));
                }
            }
            "builtin::i64_mul_wide_u" => {
                self.generate_args(func, args, type_table, ctx, builder);
                func.instruction(&Instruction::I64MulWideU);
                if !ctx.skip_tuple_wrap {
                    let tuple_type_idx =
                        self.get_struct_or_tuple_type_idx(expr.type_id, type_table);
                    func.instruction(&Instruction::StructNew(tuple_type_idx));
                }
            }
            "builtin::i64_mul_wide_s" => {
                self.generate_args(func, args, type_table, ctx, builder);
                func.instruction(&Instruction::I64MulWideS);
                if !ctx.skip_tuple_wrap {
                    let tuple_type_idx =
                        self.get_struct_or_tuple_type_idx(expr.type_id, type_table);
                    func.instruction(&Instruction::StructNew(tuple_type_idx));
                }
            }
            "builtin::i32_clz" => {
                self.generate_args(func, args, type_table, ctx, builder);
                func.instruction(&Instruction::I32Clz);
            }
            "builtin::i64_clz" => {
                self.generate_args(func, args, type_table, ctx, builder);
                func.instruction(&Instruction::I64Clz);
            }
            "builtin::i64_reinterpret_f64" => {
                self.generate_args(func, args, type_table, ctx, builder);
                func.instruction(&Instruction::I64ReinterpretF64);
            }
            "builtin::f64_reinterpret_i64" => {
                self.generate_args(func, args, type_table, ctx, builder);
                func.instruction(&Instruction::F64ReinterpretI64);
            }
            "builtin::i32_reinterpret_f32" => {
                self.generate_args(func, args, type_table, ctx, builder);
                func.instruction(&Instruction::I32ReinterpretF32);
            }
            "builtin::f32_reinterpret_i32" => {
                self.generate_args(func, args, type_table, ctx, builder);
                func.instruction(&Instruction::F32ReinterpretI32);
            }
            // Float math operations (single-argument)
            "builtin::f32_abs" => {
                self.generate_args(func, args, type_table, ctx, builder);
                func.instruction(&Instruction::F32Abs);
            }
            "builtin::f64_abs" => {
                self.generate_args(func, args, type_table, ctx, builder);
                func.instruction(&Instruction::F64Abs);
            }
            "builtin::f32_ceil" => {
                self.generate_args(func, args, type_table, ctx, builder);
                func.instruction(&Instruction::F32Ceil);
            }
            "builtin::f64_ceil" => {
                self.generate_args(func, args, type_table, ctx, builder);
                func.instruction(&Instruction::F64Ceil);
            }
            "builtin::f32_floor" => {
                self.generate_args(func, args, type_table, ctx, builder);
                func.instruction(&Instruction::F32Floor);
            }
            "builtin::f64_floor" => {
                self.generate_args(func, args, type_table, ctx, builder);
                func.instruction(&Instruction::F64Floor);
            }
            "builtin::f32_trunc" => {
                self.generate_args(func, args, type_table, ctx, builder);
                func.instruction(&Instruction::F32Trunc);
            }
            "builtin::f64_trunc" => {
                self.generate_args(func, args, type_table, ctx, builder);
                func.instruction(&Instruction::F64Trunc);
            }
            "builtin::f32_nearest" => {
                self.generate_args(func, args, type_table, ctx, builder);
                func.instruction(&Instruction::F32Nearest);
            }
            "builtin::f64_nearest" => {
                self.generate_args(func, args, type_table, ctx, builder);
                func.instruction(&Instruction::F64Nearest);
            }
            "builtin::f32_sqrt" => {
                self.generate_args(func, args, type_table, ctx, builder);
                func.instruction(&Instruction::F32Sqrt);
            }
            "builtin::f64_sqrt" => {
                self.generate_args(func, args, type_table, ctx, builder);
                func.instruction(&Instruction::F64Sqrt);
            }
            // Float math operations (two-argument)
            "builtin::f32_min" => {
                self.generate_args(func, args, type_table, ctx, builder);
                func.instruction(&Instruction::F32Min);
            }
            "builtin::f64_min" => {
                self.generate_args(func, args, type_table, ctx, builder);
                func.instruction(&Instruction::F64Min);
            }
            "builtin::f32_max" => {
                self.generate_args(func, args, type_table, ctx, builder);
                func.instruction(&Instruction::F32Max);
            }
            "builtin::f64_max" => {
                self.generate_args(func, args, type_table, ctx, builder);
                func.instruction(&Instruction::F64Max);
            }
            "builtin::f32_copysign" => {
                self.generate_args(func, args, type_table, ctx, builder);
                func.instruction(&Instruction::F32Copysign);
            }
            "builtin::f64_copysign" => {
                self.generate_args(func, args, type_table, ctx, builder);
                func.instruction(&Instruction::F64Copysign);
            }
            "builtin::call_indirect_stdout_write_via_stream" => {
                self.generate_args(func, args, type_table, ctx, builder);
                func.instruction(&Instruction::I32Const(2048));
                let stdout_func = build_local_alias_name("cli", "Stdout", "write_via_stream");
                let func_idx = builder.func_idx(&stdout_func);
                func.instruction(&Instruction::Call(func_idx));
                let subtask_local = ctx.get_local("__subtask").expect(
                    "__subtask should be pre-allocated for functions with Stdout/Stderr effects",
                );
                func.instruction(&Instruction::LocalSet(subtask_local));
            }
            "builtin::call_indirect_stderr_write_via_stream" => {
                self.generate_args(func, args, type_table, ctx, builder);
                func.instruction(&Instruction::I32Const(2048));
                let stderr_func = build_local_alias_name("cli", "Stderr", "write_via_stream");
                let func_idx = builder.func_idx(&stderr_func);
                func.instruction(&Instruction::Call(func_idx));
                let subtask_local = ctx.get_local("__subtask").expect(
                    "__subtask should be pre-allocated for functions with Stdout/Stderr effects",
                );
                func.instruction(&Instruction::LocalSet(subtask_local));
            }
            // Builtins with canonical function mappings - generate regular function calls
            // These are declared with #[canonical(...)] in builtin.wado
            "builtin::realloc"
            | "builtin::f64_to_buffer"
            | "builtin::f32_to_buffer"
            | "builtin::stream_new"
            | "builtin::stream_write"
            | "builtin::stream_drop_writable"
            | "builtin::stream_drop_readable"
            | "builtin::future_new"
            | "builtin::future_write"
            | "builtin::future_drop_writable"
            | "builtin::future_drop_readable"
            | "builtin::task_return"
            | "builtin::waitable_set_new"
            | "builtin::waitable_join"
            | "builtin::waitable_set_wait"
            | "builtin::subtask_drop"
            // Libm builtins (f64)
            | "builtin::f64_sin"
            | "builtin::f64_cos"
            | "builtin::f64_tan"
            | "builtin::f64_asin"
            | "builtin::f64_acos"
            | "builtin::f64_atan"
            | "builtin::f64_atan2"
            | "builtin::f64_sinh"
            | "builtin::f64_cosh"
            | "builtin::f64_tanh"
            | "builtin::f64_asinh"
            | "builtin::f64_acosh"
            | "builtin::f64_atanh"
            | "builtin::f64_exp"
            | "builtin::f64_exp2"
            | "builtin::f64_expm1"
            | "builtin::f64_ln"
            | "builtin::f64_log2"
            | "builtin::f64_log10"
            | "builtin::f64_ln1p"
            | "builtin::f64_pow"
            | "builtin::f64_cbrt"
            | "builtin::f64_hypot"
            | "builtin::f64_fmod"
            // Libm builtins (f32)
            | "builtin::f32_sin"
            | "builtin::f32_cos"
            | "builtin::f32_tan"
            | "builtin::f32_asin"
            | "builtin::f32_acos"
            | "builtin::f32_atan"
            | "builtin::f32_atan2"
            | "builtin::f32_sinh"
            | "builtin::f32_cosh"
            | "builtin::f32_tanh"
            | "builtin::f32_asinh"
            | "builtin::f32_acosh"
            | "builtin::f32_atanh"
            | "builtin::f32_exp"
            | "builtin::f32_exp2"
            | "builtin::f32_expm1"
            | "builtin::f32_ln"
            | "builtin::f32_log2"
            | "builtin::f32_log10"
            | "builtin::f32_ln1p"
            | "builtin::f32_pow"
            | "builtin::f32_cbrt"
            | "builtin::f32_hypot"
            | "builtin::f32_fmod" => {
                self.generate_args(func, args, type_table, ctx, builder);
                // Look up the canonical name from the builtin registry
                let func_name = builtin_name.strip_prefix("builtin::").unwrap();
                let builtin_info = self
                    .project
                    .builtin_registry
                    .get(func_name)
                    .unwrap_or_else(|| panic!("builtin not found in registry: {func_name}"));
                let canonical_name = builtin_info
                    .canonical_name
                    .as_ref()
                    .unwrap_or_else(|| panic!("builtin {func_name} has no canonical name"));
                let func_idx = builder.func_idx(canonical_name);
                func.instruction(&Instruction::Call(func_idx));
            }
            _ => panic!(
                "unknown builtin function: {builtin_name}, which should be handled in Codegen::generate_builtin_call()"
            ),
        }
    }

    /// Generate code for variant constructors (Ok, Err, Some, None).
    /// Returns `true` if the call was handled, `false` otherwise.
    fn generate_variant_constructor(
        &self,
        func_name: &str,
        args: &[TirExpr],
        func: &mut Function,
        type_table: &TypeTable,
        ctx: &mut FunctionContext,
        builder: &CoreModuleBuilder,
    ) -> bool {
        match func_name {
            "Ok" => {
                let is_unit_payload = args.is_empty()
                    || (args.len() == 1 && matches!(&args[0].kind, TirExprKind::Unit));
                if is_unit_payload {
                    func.instruction(&Instruction::I32Const(0));
                } else {
                    self.generate_args(func, args, type_table, ctx, builder);
                }
                true
            }
            "Err" => {
                let is_unit_payload = args.is_empty()
                    || (args.len() == 1 && matches!(&args[0].kind, TirExprKind::Unit));
                if is_unit_payload {
                    func.instruction(&Instruction::I32Const(1));
                } else {
                    self.generate_args(func, args, type_table, ctx, builder);
                }
                true
            }
            "Some" => {
                self.generate_args(func, args, type_table, ctx, builder);
                true
            }
            "None" => {
                func.instruction(&Instruction::RefNull(HeapType::Abstract {
                    shared: false,
                    ty: AbstractHeapType::None,
                }));
                true
            }
            _ => false,
        }
    }

    fn allocate_precomputed_scratch_locals(
        &self,
        tir_func: &TirFunction,
        type_table: &TypeTable,
        ctx: &mut FunctionContext,
    ) {
        for scratch in &tir_func.scratch_locals {
            let val_type = self.type_id_to_valtype(type_table, scratch.type_id);
            ctx.alloc_local(&scratch.name, val_type);
        }

        if !tir_func.copy_source_types.is_empty() {
            self.allocate_copy_source_locals(&tir_func.copy_source_types, type_table, ctx);
        }

        for (&closure_type_id, &count) in &tir_func.indirect_call_counts {
            if let Some(struct_type_idx) =
                self.get_closure_struct_type_idx(closure_type_id, type_table)
            {
                for i in 0..count {
                    let name = format!("__indirect_call_{struct_type_idx}_{i}");
                    ctx.alloc_local(
                        &name,
                        ValType::Ref(RefType {
                            nullable: true,
                            heap_type: HeapType::Concrete(struct_type_idx),
                        }),
                    );
                }
            }
        }

        for (i, &type_id) in tir_func.let_pattern_types.iter().enumerate() {
            let val_type = self.type_id_to_valtype(type_table, type_id);
            let name = format!("__let_pattern_temp_{i}");
            ctx.alloc_local(&name, val_type);
        }

        for (i, &type_id) in tir_func.match_scrutinee_types.iter().enumerate() {
            let val_type = self.type_id_to_valtype(type_table, type_id);
            let type_key = format!("match_{type_id:?}");
            let name = format!("__match_scrutinee_{type_key}_{i}");
            ctx.alloc_local(&name, val_type);
        }
    }

    fn allocate_copy_source_locals(
        &self,
        copy_source_types: &std::collections::HashSet<TypeId>,
        type_table: &TypeTable,
        ctx: &mut FunctionContext,
    ) {
        for &type_id in copy_source_types {
            match type_table.get(type_id) {
                ResolvedType::Struct {
                    name,
                    module_source,
                    ..
                } => {
                    if let Some(info) = self.lookup_struct_type(name, module_source) {
                        let local_name = format!("__copy_source_{}", info.type_idx);
                        let local_idx =
                            ctx.alloc_local(&local_name, CopyContext::nullable_ref(info.type_idx));
                        ctx.copy_context
                            .register_struct_copy_local(info.type_idx, local_idx);
                    }
                }
                ResolvedType::Tuple(elements) => {
                    if let Some(type_idx) = self.get_tuple_type_idx(elements) {
                        let local_idx = ctx.alloc_local(
                            &format!("__copy_source_{type_idx}"),
                            CopyContext::nullable_ref(type_idx),
                        );
                        ctx.copy_context
                            .register_struct_copy_local(type_idx, local_idx);
                    }
                }
                ResolvedType::GenericInstance {
                    name, type_args, ..
                } if name == "Array" && type_args.len() == 1 => {
                    let elem_type = type_args[0];
                    if let Some(&raw_array_type_idx) = self.array_types.get(&elem_type) {
                        // Allocate locals for the Array struct wrapper
                        let struct_source = if let Some(array_struct_type_idx) =
                            self.lookup_array_struct_type(elem_type, type_table)
                        {
                            ctx.alloc_local(
                                &format!("__copy_array_struct_source_{raw_array_type_idx}"),
                                CopyContext::nullable_ref(array_struct_type_idx),
                            )
                        } else {
                            0 // Fallback, should not happen
                        };

                        let source = ctx.alloc_local(
                            &format!("__copy_array_source_{raw_array_type_idx}"),
                            CopyContext::nullable_ref(raw_array_type_idx),
                        );
                        let dest = ctx.alloc_local(
                            &format!("__copy_array_dest_{raw_array_type_idx}"),
                            CopyContext::nullable_ref(raw_array_type_idx),
                        );
                        let counter = ctx.alloc_local(
                            &format!("__copy_array_counter_{raw_array_type_idx}"),
                            ValType::I32,
                        );
                        let len = ctx.alloc_local(
                            &format!("__copy_array_len_{raw_array_type_idx}"),
                            ValType::I32,
                        );
                        ctx.copy_context.register_array_copy_locals(
                            raw_array_type_idx,
                            ArrayCopyLocals {
                                struct_source,
                                source,
                                dest,
                                counter,
                                len,
                            },
                        );
                    }
                }
                ResolvedType::Option(inner) => {
                    // Option<T> needs a copy source local if T needs copying
                    if self.needs_value_copy(*inner, type_table) {
                        // Get the inner type's Wasm type index for keying
                        let inner_valtype = self.type_id_to_valtype(type_table, *inner);
                        if let ValType::Ref(ref_type) = inner_valtype
                            && let Some(inner_type_idx) =
                                CopyContext::heap_type_to_idx(ref_type.heap_type)
                        {
                            // Option's ValType is nullable version of inner
                            let option_valtype = ValType::Ref(RefType {
                                nullable: true,
                                ..ref_type
                            });
                            let local_idx = ctx.alloc_local(
                                &format!("__copy_option_source_{inner_type_idx}"),
                                option_valtype,
                            );
                            ctx.copy_context
                                .register_option_copy_local(inner_type_idx, local_idx);
                        }
                    }
                }
                ResolvedType::Variant { name, .. } => {
                    // Variants need a copy local for the variant struct
                    let variant_types = &self.variant_types;
                    if let Some(info) = variant_types.get(name) {
                        let base_type_idx = info.base_type_idx;
                        let local_idx = ctx.alloc_local(
                            &format!("__copy_source_{base_type_idx}"),
                            CopyContext::nullable_ref(base_type_idx),
                        );
                        ctx.copy_context
                            .register_struct_copy_local(base_type_idx, local_idx);
                    }
                }
                _ => {
                    // Other types don't need special copy locals
                }
            }
        }
    }

    /// Get the closure struct type index for a closure type (by function signature).
    fn get_closure_struct_type_idx(
        &self,
        closure_type_id: TypeId,
        type_table: &TypeTable,
    ) -> Option<u32> {
        // The TypeId is a Function type - extract params and return_type
        if let ResolvedType::Function {
            params,
            return_type,
            ..
        } = type_table.get(closure_type_id)
        {
            let key = (params.clone(), *return_type);
            self.canonical_closure_types
                .get(&key)
                .map(|(_, _, struct_type_idx)| *struct_type_idx)
        } else {
            None
        }
    }

    /// Convert a WASI function type to Core Wasm params
    ///
    /// For async functions, an extra i32 param (outptr) is added per Component Model ABI.
    /// For sync functions with complex return types, an outptr is also added.
    /// For sync functions with simple return types, params are mapped directly.
    fn wasi_func_to_core_params(&self, func: &WasiFunctionInfo) -> Vec<ValType> {
        let mut params: Vec<ValType> = func
            .params
            .iter()
            .map(|(_, ty)| {
                // Resolve type aliases (e.g., Mark -> u64) before converting to ValType
                let resolved_ty = self.project.wasi_registry.resolve_type(ty);
                wasi_type_to_valtype(&resolved_ty)
            })
            .collect();

        // Async functions have an additional outptr parameter for the result
        if func.is_async {
            params.push(ValType::I32); // outptr
        }
        // Sync functions with complex return types also need an outptr
        // Resolve type aliases first to correctly detect complex types
        else if let Some(ret_ty) = &func.return_type {
            let resolved_ret_ty = self.project.wasi_registry.resolve_type(ret_ty);
            if return_type_requires_outptr(&resolved_ret_ty) {
                params.push(ValType::I32); // outptr
            }
        }

        params
    }

    /// Convert a WASI function type to Core Wasm results
    ///
    /// For async functions, the result is always i32 (subtask handle).
    /// For sync functions with complex return types, returns nothing (result via outptr).
    /// For sync functions with simple return types, the return type is mapped directly.
    fn wasi_func_to_core_results(&self, func: &WasiFunctionInfo) -> Vec<ValType> {
        if func.is_async {
            // Async functions return a subtask handle (i32)
            vec![ValType::I32]
        } else if let Some(ret_ty) = &func.return_type {
            // Resolve type aliases (e.g., Mark -> u64) before checking/converting
            let resolved_ty = self.project.wasi_registry.resolve_type(ret_ty);
            // Complex types are returned via outptr, so no direct return value
            if return_type_requires_outptr(&resolved_ty) {
                vec![]
            } else {
                vec![wasi_type_to_valtype(&resolved_ty)]
            }
        } else {
            vec![]
        }
    }

    /// Convert a builtin function type to Core Wasm params
    fn builtin_func_to_core_params(&self, func: &BuiltinFunctionInfo) -> Vec<ValType> {
        func.params
            .iter()
            .map(|(_, ty)| type_id_to_valtype(*ty))
            .collect()
    }

    /// Convert a builtin function type to Core Wasm results
    fn builtin_func_to_core_results(&self, func: &BuiltinFunctionInfo) -> Vec<ValType> {
        if func.diverges || func.return_type == TypeTable::UNIT {
            // Diverging or void functions have no return type
            vec![]
        } else {
            vec![type_id_to_valtype(func.return_type)]
        }
    }

    /// Convert a world export function type to Core Wasm params
    ///
    /// NOTE: This function is used to declare the CORE function type for world exports.
    /// For async exports, the actual params depend on the user's TIR function.
    /// We return empty here because the function type is later overridden by the
    /// user's TIR function if one exists.
    fn world_export_to_core_params(&self, _export: &WorldExportInfo) -> Vec<ValType> {
        // World export core params are handled dynamically based on the TIR function.
        // For worlds where user provides the function (like CLI Command's run or HTTP Service's handle),
        // the actual params come from the user's TIR function.
        vec![]
    }

    /// Convert a world export function type to Core Wasm results
    ///
    /// For async exports, there's no return (result passed via `task_return`).
    /// For sync exports, the return type is mapped directly.
    fn world_export_to_core_results(&self, export: &WorldExportInfo) -> Vec<ValType> {
        if export.is_async {
            // Async exports have no return in core (use task_return)
            vec![]
        } else if let Some(ret_ty) = &export.return_type {
            // Resolve type aliases (e.g., newtypes) before converting to ValType
            let resolved_ty = self.project.wasi_registry.resolve_type(ret_ty);
            vec![wasi_type_to_valtype(&resolved_ty)]
        } else {
            vec![]
        }
    }

    /// Build the memory module (provides shared memory and realloc for all core modules)
    fn build_memory_module(&self, strip_names: bool) -> Vec<u8> {
        let mut module = Module::new();

        // Type section: realloc type
        let mut types = TypeSection::new();
        types.ty().function(
            [ValType::I32, ValType::I32, ValType::I32, ValType::I32],
            [ValType::I32],
        );
        module.section(&types);

        // Function section
        let mut functions = FunctionSection::new();
        functions.function(0); // realloc uses type 0
        module.section(&functions);

        // Memory section
        // Minimum 17 pages to satisfy the float-to-string module's memory requirements
        // (the bundled module needs ~1MB for its data segment)
        let mut memories = MemorySection::new();
        memories.memory(MemoryType {
            minimum: 17,
            maximum: None,
            memory64: false,
            shared: false,
            page_size_log2: None,
        });
        module.section(&memories);

        // Export section
        let mut exports = ExportSection::new();
        exports.export("memory", ExportKind::Memory, 0);
        exports.export("realloc", ExportKind::Func, 0);
        module.section(&exports);

        // Code section: realloc function
        let mut code = CodeSection::new();
        let mut realloc_func = Function::new([]);
        realloc_func.instruction(&Instruction::I32Const(1024));
        realloc_func.instruction(&Instruction::End);
        code.function(&realloc_func);
        module.section(&code);

        // Note: String literals are stored in the main module's passive data segment
        // for use with array.new_data. The memory module doesn't need a copy since
        // realloc returns offset 1024+ and no code reads from offset 0.

        // Name section (skip in size-optimized builds)
        if !strip_names {
            let mut names = NameSection::new();
            let mut func_names = NameMap::new();
            func_names.append(0, "realloc");
            names.functions(&func_names);
            let mut type_names = NameMap::new();
            type_names.append(0, "realloc");
            names.types(&type_names);
            module.section(&names);
        }

        module.finish()
    }
}

/// Convert a `PrimitiveType` to its corresponding Wasm `ValType`.
/// Used for boxing primitives in references.
fn primitive_to_valtype(prim: &PrimitiveType) -> ValType {
    match prim {
        PrimitiveType::I8
        | PrimitiveType::I16
        | PrimitiveType::I32
        | PrimitiveType::U8
        | PrimitiveType::U16
        | PrimitiveType::U32
        | PrimitiveType::Bool
        | PrimitiveType::Char => ValType::I32,
        PrimitiveType::I64 | PrimitiveType::U64 => ValType::I64,
        PrimitiveType::F32 => ValType::F32,
        PrimitiveType::F64 => ValType::F64,
        PrimitiveType::I128 | PrimitiveType::U128 => {
            panic!("i128/u128 references not yet supported")
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::compiler_host::InMemoryCompilerHost;

    #[tokio::test]
    async fn test_generate_binary() {
        let host = InMemoryCompilerHost::new();
        // Use O0 to avoid DCE removing unused code in this simple smoke test
        let result = crate::compile_with_host(
            r#"
            fn add(a: i32, b: i32) -> i32 {
                return a + b;
            }

            fn run() {
                let result = add(1, 2);
            }
        "#,
            &host,
            None,
            crate::OptLevel::O0,
        )
        .await
        .expect("compilation failed");

        // Verify it starts with Wasm magic number
        let wasm = result.wasm;
        assert!(wasm.len() > 8);
        assert_eq!(&wasm[0..4], b"\0asm");
    }
}
