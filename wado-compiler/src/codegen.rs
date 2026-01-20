// Code generator for Wado
// Generates Component Model WebAssembly using wasm-encoder
// Targets WASI P3 (0.3.0-rc-2025-09-16) with native stream<T> types

use std::cell::RefCell;
use std::rc::Rc;

use crate::ast::Type;
use crate::builtin_registry::{BuiltinFunctionInfo, BuiltinRegistry};
use crate::bundled::wado_bundled_wasm;
use crate::component_model::{
    WasiFunctionInfo, WasiRegistry, build_local_alias_name, is_wasi_function_supported,
    return_type_requires_outptr, wasi_type_to_valtype,
};
use crate::name::{FreeFunctionName, FunctionId, MethodName, StructName, build_core_internal_name};
use crate::optimize::{CanonBuiltin, WasiEffect};
use crate::project::Project;
use crate::symbol::SymbolTable;
use crate::tir::{
    PrimitiveType, ResolvedType, TirBinaryOp, TirBlock, TirCapture, TirExpr, TirExprKind,
    TirFunction, TirModule, TirPattern, TirStmt, TirStmtKind, TirUnaryOp, TypeId, TypeTable,
};
use crate::wasm_builder::{ComponentModelContext, CoreModuleBuilder};
use crate::wasm_postprocess;
use crate::world_registry::{WorldExportInfo, WorldRegistry};
use heck::ToKebabCase;
use indexmap::IndexMap;
use std::collections::{HashMap, HashSet};
use wasm_encoder::{
    AbstractHeapType, Alias, BranchHint, BranchHints, CanonicalOption, CodeSection,
    ComponentBuilder, ComponentExportKind, ComponentOuterAliasKind, ComponentValType, ConstExpr,
    DataCountSection, DataSection, DataSegment, DataSegmentMode, ElementSection, Elements,
    ExportKind, ExportSection, FieldType, Function, FunctionSection, HeapType, InstanceType,
    Instruction, MemArg, MemorySection, MemoryType, Module, ModuleArg, NameMap, NameSection,
    PrimitiveValType, RefType, StorageType, TypeBounds, TypeSection, ValType,
};
use wasmparser::{Validator, WasmFeatures};

/// Module path for the String struct in core/prelude
/// Used to avoid repeated allocations when looking up String type
const STRING_MODULE_PATH: &[&str] = &["core", "prelude"];

/// Helper to convert `STRING_MODULE_PATH` to Vec<String> (for APIs requiring owned strings)
fn string_module_path() -> Vec<String> {
    STRING_MODULE_PATH
        .iter()
        .map(|s| (*s).to_string())
        .collect()
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
    all_tir_modules: &'a IndexMap<Vec<String>, TirModule>,
    symbols: &'a SymbolTable,
    string_data: &'a [u8],
    project: &'a Project,
    module_name: &'a str,
    /// WASI functions that are available (lowered at component level)
    /// These are the local alias names (e.g., "`wasi:cli/Stdout::write_via_stream`")
    available_wasi_funcs: &'a HashSet<String>,
}

/// Code generator that produces Component Model components
/// Targets WASI P3
pub struct Codegen {
    string_literals: Vec<String>,
    /// Registry of WASI imports from lib/wasi/*.wado
    wasi_registry: WasiRegistry,
    /// Registry of builtin function signatures from lib/core/builtin.wado
    builtin_registry: BuiltinRegistry,
    /// Registry of world definitions from lib/wasi/*.wado
    world_registry: WorldRegistry,
    /// Type index for string-array (GC array<u8>), set when types are defined
    string_array_type_idx: u32,
    /// Registry of user-defined struct types (keyed by `StructName` for type safety)
    struct_types: HashMap<StructName, StructTypeInfo>,
    /// Registry of tuple types (keyed by element `TypeIds`, maps to GC struct type index)
    /// Uses `RefCell` for lazy registration during codegen
    tuple_types: RefCell<HashMap<Vec<TypeId>, u32>>,
    /// Registry of raw array types (keyed by element `TypeId`, maps to GC array type index)
    /// These are the underlying `builtin::array`<T> types used in Array<T>.repr
    array_types: HashMap<TypeId, u32>,
    /// Registry of Array<T> struct types (keyed by element `TypeId`, maps to GC struct type index)
    /// Array<T> is a struct with fields: repr (ref to GC array), used (i32)
    array_struct_types: HashMap<TypeId, u32>,
    /// Registry of box types for primitive references (keyed by `ValType`, maps to GC struct type index)
    /// Box types are single-field mutable structs that allow references to primitives
    box_types: HashMap<ValType, u32>,
    /// Counter for generating unique closure IDs
    closure_counter: RefCell<u32>,
    /// Registry of closure environment types
    /// Key: vector of (`type_id`, `is_mut`) for each capture
    /// Value: (`env_type_idx`, `env_type_name`)
    closure_env_types: RefCell<HashMap<Vec<(TypeId, bool)>, (u32, String)>>,
    /// Registry of closure struct types (env + funcref pair)
    /// Key: (`env_type_idx`, `fn_type_idx`)
    /// Value: `closure_struct_type_idx`
    #[allow(dead_code)]
    closure_struct_types: RefCell<HashMap<(u32, u32), u32>>,
    /// Registry of canonical closure types based on user-visible function signature.
    /// Used for function type parameters (e.g., `fn(i32) -> i32`).
    /// Key: (`param_type_ids`, `return_type_id`)
    /// Value: (`canonical_fn_type_idx`, `canonical_fn_type_name`, `canonical_closure_struct_type_idx`)
    canonical_closure_types: RefCell<HashMap<(Vec<TypeId>, TypeId), (u32, String, u32)>>,
    /// Pending closure implementation functions to generate
    /// (`closure_id`, captures, params, body, `return_type`, `env_type_idx`, `closure_type_idx`)
    pending_closures: RefCell<Vec<ClosureInfo>>,
    /// Counter for tracking which closure we're generating during codegen.
    /// Reset before codegen starts, incremented each time we encounter a Closure expression.
    /// Must match the order in which closures were collected by `collect_closures_from_module`.
    closure_codegen_counter: RefCell<u32>,
    /// Registry of custom variant types
    /// Key: variant name (e.g., "Shape")
    /// Value: `VariantTypeInfo` with struct type index and case metadata
    variant_types: RefCell<HashMap<String, VariantTypeInfo>>,
}

/// Information about a custom variant type's Wasm GC representation
#[derive(Clone, Debug)]
struct VariantTypeInfo {
    /// The GC struct type index for this variant
    struct_type_idx: u32,
    /// Information about each case: (`case_name`, `field_count`)
    #[allow(dead_code)]
    cases: Vec<(String, usize)>,
    /// Field types for the payload fields (after the tag)
    /// Index 0 corresponds to field index 1 in the struct (field 0 is the tag)
    field_types: Vec<ValType>,
}

/// Information about a closure to be generated
#[derive(Clone)]
struct ClosureInfo {
    /// Unique closure ID
    #[allow(dead_code)]
    id: u32,
    /// Captured variables from outer scope
    captures: Vec<TirCapture>,
    /// Closure parameters (name, type)
    params: Vec<(String, TypeId)>,
    /// Closure body expression
    body: TirExpr,
    /// Return type of the closure
    return_type: TypeId,
    /// Wasm type index for the environment struct
    env_type_idx: u32,
    /// Wasm type index for the closure function type (env + params -> result)
    #[allow(dead_code)]
    fn_type_idx: u32,
    /// Wasm type name for the closure function type (needed for `define_func`)
    fn_type_name: String,
    /// Wasm type index for the closure struct type (env + funcref)
    closure_struct_type_idx: u32,
    /// Wasm function index for the closure implementation function
    func_idx: u32,
    /// Function name for the closure implementation
    func_name: String,
}

/// Collected closure during TIR scan (before type registration)
struct CollectedClosure {
    captures: Vec<TirCapture>,
    params: Vec<(String, TypeId)>,
    body: TirExpr,
    return_type: TypeId,
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
    /// Stack of (`extra_depth`, `break_offset`) for each loop level.
    /// `extra_depth`: incremented by if statements inside the loop
    /// `break_offset`: 1 for while/loop, 2 for for loops (because for loops have an extra body block)
    /// For break: use `break_offset` + `extra_depth`
    /// For continue: use `extra_depth`
    loop_info: Vec<(u32, u32)>,
    /// Counter for generating unique for-of local names (to support nested for-of loops)
    for_of_counter: u32,
    /// Local indices that have their address taken (&x or &mut x).
    /// For mutable primitives, these locals store a box reference instead of the raw value.
    address_taken_locals: std::collections::HashSet<u32>,
    /// Map from local index to its box type index (for address-taken primitive locals)
    local_box_types: HashMap<u32, u32>,
    /// Closure environment type index (set when generating closure implementation function)
    closure_env_type_idx: Option<u32>,
    /// Closure captures (set when generating closure implementation function)
    closure_captures: Vec<TirCapture>,
    /// Offset to add to local indices for closure functions (typically 1 to skip env param)
    local_index_offset: u32,
    /// Map from local index to closure id (for closures stored in locals)
    local_closure_ids: HashMap<u32, u32>,
    /// Counter for generating unique `IndirectCall` temp locals (to support nested closure calls)
    indirect_call_counter: u32,
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
            for_of_counter: 0,
            address_taken_locals: std::collections::HashSet::new(),
            local_box_types: HashMap::new(),
            closure_env_type_idx: None,
            closure_captures: Vec::new(),
            local_index_offset: 0,
            local_closure_ids: HashMap::new(),
            indirect_call_counter: 0,
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
            for_of_counter: 0,
            address_taken_locals: std::collections::HashSet::new(),
            local_box_types: HashMap::new(),
            closure_env_type_idx: None,
            closure_captures: Vec::new(),
            indirect_call_counter: 0,
            local_index_offset: 0,
            local_closure_ids: HashMap::new(),
        }
    }

    /// Set closure context for generating a closure implementation function.
    /// This enables `TirExprKind::Capture` to generate proper struct.get instructions.
    /// Also sets `local_index_offset` to 1 to account for the env parameter at index 0.
    fn set_closure_info(&mut self, env_type_idx: u32, captures: &[TirCapture]) {
        self.closure_env_type_idx = Some(env_type_idx);
        self.closure_captures = captures.to_vec();
        self.local_index_offset = 1; // Skip env param at index 0
    }

    /// Reset for-of counter (called between pre-allocation and code generation phases)
    fn reset_for_of_counter(&mut self) {
        self.for_of_counter = 0;
    }

    /// Get next for-of loop ID and increment counter
    fn next_for_of_id(&mut self) -> u32 {
        let id = self.for_of_counter;
        self.for_of_counter += 1;
        id
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

impl Default for Codegen {
    fn default() -> Self {
        Self::new()
    }
}

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

impl Codegen {
    /// Create a new code generator with registries built from stdlib
    pub fn new() -> Self {
        let (wasi_registry, world_registry) = WasiRegistry::build_from_stdlib();
        let builtin_registry = BuiltinRegistry::build_from_stdlib();

        Self {
            string_literals: Vec::new(),
            wasi_registry,
            builtin_registry,
            world_registry,
            string_array_type_idx: 0, // Set when types are defined
            struct_types: HashMap::new(),
            tuple_types: RefCell::new(HashMap::new()),
            array_types: HashMap::new(),
            array_struct_types: HashMap::new(),
            box_types: HashMap::new(),
            closure_counter: RefCell::new(0),
            closure_env_types: RefCell::new(HashMap::new()),
            closure_struct_types: RefCell::new(HashMap::new()),
            canonical_closure_types: RefCell::new(HashMap::new()),
            pending_closures: RefCell::new(Vec::new()),
            closure_codegen_counter: RefCell::new(0),
            variant_types: RefCell::new(HashMap::new()),
        }
    }

    /// Create a new code generator (backwards compatible)
    pub fn new_with_source(_source_code: String) -> Self {
        Self::new()
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

    /// Look up a struct type by name and module path.
    /// Tries qualified `StructName` first, falls back to simple name (empty `module_path`).
    fn lookup_struct_type(&self, name: &str, module_path: &[String]) -> Option<&StructTypeInfo> {
        if !module_path.is_empty() {
            // Try qualified name first
            let qualified = StructName::from_path_and_name(module_path, name);
            if let Some(info) = self.struct_types.get(&qualified) {
                return Some(info);
            }
        }
        // Fall back to simple name (empty module_path)
        let simple = StructName {
            module_path: Vec::new(),
            name: name.to_string(),
        };
        self.struct_types.get(&simple)
    }

    /// Mangle a type name for use in struct names (e.g., i32 for Box<i32>)
    fn mangle_type_for_struct_name(&self, type_id: TypeId, type_table: &TypeTable) -> String {
        match type_table.get(type_id) {
            ResolvedType::Primitive(prim) => match prim {
                PrimitiveType::I8 => "i8".to_string(),
                PrimitiveType::I16 => "i16".to_string(),
                PrimitiveType::I32 => "i32".to_string(),
                PrimitiveType::I64 => "i64".to_string(),
                PrimitiveType::I128 => "i128".to_string(),
                PrimitiveType::U8 => "u8".to_string(),
                PrimitiveType::U16 => "u16".to_string(),
                PrimitiveType::U32 => "u32".to_string(),
                PrimitiveType::U64 => "u64".to_string(),
                PrimitiveType::U128 => "u128".to_string(),
                PrimitiveType::F32 => "f32".to_string(),
                PrimitiveType::F64 => "f64".to_string(),
                PrimitiveType::Bool => "bool".to_string(),
                PrimitiveType::Char => "char".to_string(),
            },
            ResolvedType::Unit => "unit".to_string(),
            ResolvedType::String => "String".to_string(),
            ResolvedType::Struct { name, .. } => name.clone(),
            ResolvedType::GenericInstance {
                name, type_args, ..
            } => {
                let args: Vec<String> = type_args
                    .iter()
                    .map(|t| self.mangle_type_for_struct_name(*t, type_table))
                    .collect();
                format!("{}<{}>", name, args.join(","))
            }
            // Function types: Fn<paramCount,returnType>
            ResolvedType::Function {
                params,
                return_type,
                ..
            } => {
                let ret_name = self.mangle_type_for_struct_name(*return_type, type_table);
                format!("Fn<{},{}>", params.len(), ret_name)
            }
            ResolvedType::Tuple(elems) => {
                let elem_names: Vec<String> = elems
                    .iter()
                    .map(|t| self.mangle_type_for_struct_name(*t, type_table))
                    .collect();
                format!("Tuple<{}>", elem_names.join(","))
            }
            ResolvedType::Option(inner) => {
                let inner_name = self.mangle_type_for_struct_name(*inner, type_table);
                format!("Option<{inner_name}>")
            }
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
                // For references, use the inner type's name
                self.mangle_type_for_struct_name(*inner, type_table)
            }
            _ => "unknown".to_string(),
        }
    }

    /// Extract struct names that a type depends on (for field types)
    fn get_struct_dependencies(type_table: &TypeTable, type_id: TypeId) -> Vec<String> {
        match type_table.get(type_id) {
            ResolvedType::Struct { name, .. } => vec![name.clone()],
            ResolvedType::GenericInstance {
                name, type_args, ..
            } => {
                // Get dependencies from type arguments
                let mut deps = vec![name.clone()];
                for arg in type_args {
                    deps.extend(Self::get_struct_dependencies(type_table, *arg));
                }
                deps
            }
            ResolvedType::BuiltinArray(inner)
            | ResolvedType::Option(inner)
            | ResolvedType::Ref(inner)
            | ResolvedType::MutRef(inner)
            | ResolvedType::Stream(inner)
            | ResolvedType::Future(inner)
            | ResolvedType::Reactive(inner) => Self::get_struct_dependencies(type_table, *inner),
            ResolvedType::Result { ok, err }
            | ResolvedType::Dict {
                key: ok,
                value: err,
            } => {
                let mut deps = Self::get_struct_dependencies(type_table, *ok);
                deps.extend(Self::get_struct_dependencies(type_table, *err));
                deps
            }
            ResolvedType::Tuple(elems) => elems
                .iter()
                .flat_map(|e| Self::get_struct_dependencies(type_table, *e))
                .collect(),
            _ => vec![],
        }
    }

    /// Sort structs topologically so dependencies are registered before dependents
    fn sort_structs_topologically<'a>(
        structs: &'a [crate::tir::TirStruct],
        type_table: &TypeTable,
    ) -> Vec<&'a crate::tir::TirStruct> {
        // Build dependency graph: deps[A] = [B] means A depends on B (B must come before A)
        let struct_names: HashSet<String> = structs.iter().map(|s| s.name.clone()).collect();
        let mut deps: HashMap<String, Vec<String>> = HashMap::new();

        for s in structs {
            let mut struct_deps = Vec::new();
            for field in &s.fields {
                let field_deps = Self::get_struct_dependencies(type_table, field.type_id);
                for dep in field_deps {
                    // Only count dependencies on structs in our set
                    if struct_names.contains(&dep) && dep != s.name {
                        struct_deps.push(dep);
                    }
                }
            }
            deps.insert(s.name.clone(), struct_deps);
        }

        // Topological sort using Kahn's algorithm
        // in_degree[A] = number of dependencies A has (structs that A needs)
        let mut in_degree: HashMap<String, usize> = HashMap::new();
        for s in structs {
            let struct_deps = deps.get(&s.name).map(std::vec::Vec::len).unwrap_or(0);
            in_degree.insert(s.name.clone(), struct_deps);
        }

        // Build reverse mapping: dependents[B] = list of structs that depend on B
        let mut dependents: HashMap<String, Vec<String>> = HashMap::new();
        for (s_name, struct_deps) in &deps {
            for dep in struct_deps {
                dependents
                    .entry(dep.clone())
                    .or_default()
                    .push(s_name.clone());
            }
        }

        // Start with structs that have no dependencies
        let mut queue: Vec<String> = in_degree
            .iter()
            .filter(|&(_, deg)| *deg == 0)
            .map(|(name, _)| name.clone())
            .collect();

        let mut sorted_names = Vec::new();
        while let Some(name) = queue.pop() {
            sorted_names.push(name.clone());
            // For each struct that depends on 'name', decrement its in_degree
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

        // Map names back to structs
        let name_to_struct: HashMap<&str, &crate::tir::TirStruct> =
            structs.iter().map(|s| (s.name.as_str(), s)).collect();
        sorted_names
            .iter()
            .filter_map(|name| name_to_struct.get(name.as_str()).copied())
            .collect()
    }

    /// Generate Component Model binary Wasm from a Project.
    ///
    /// The project must have been optimized (usage fields populated) before calling this.
    pub fn generate_wasm(&mut self, project: &Project) -> Vec<u8> {
        let entry_tir = project.entry_module();
        let all_tir_modules = &project.tir_modules;
        let symbols = &project.symbols;
        let implicit_modules = &project.implicit_modules;
        let module_name = &project.module_name;

        // Collect pre-computed string literals from all TIR modules
        // Note: String DCE is performed in the optimizer, so we just collect all strings here
        for tir_module in all_tir_modules.values() {
            for s in &tir_module.string_literals {
                if !self.string_literals.contains(s) {
                    self.string_literals.push(s.clone());
                }
            }
        }

        // Generate binary Wasm from TIR
        let wasm = self.generate_component(
            entry_tir,
            all_tir_modules,
            symbols,
            implicit_modules,
            project,
            module_name,
        );

        // Validate the generated Wasm
        Self::validate_wasm(&wasm);

        wasm
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

        // Collect ALL functions from loaded TIR modules (core:*, etc.)
        // We need to include all functions because they may have transitive dependencies
        // Format: (module_path, tir_func, type_table, qualified_name)
        // Note: We store Rc<RefCell<...>> to avoid lifetime issues with temporary borrows
        let mut loaded_funcs: Vec<(
            Vec<String>,
            Rc<RefCell<TirFunction>>,
            Rc<RefCell<TypeTable>>,
            String,
        )> = Vec::new();
        for (path, tir_mod) in all_tir_modules {
            // Skip entry module (handled separately)
            if path == &entry_tir.path {
                continue;
            }
            // Skip wasi:* modules (they only contain effect declarations)
            if path.first().map(|s| s == "wasi").unwrap_or(false) {
                continue;
            }
            for func_rc in &tir_mod.functions {
                let tir_func = func_rc.borrow();
                // Skip run function
                if tir_func.name == "run" {
                    continue;
                }
                // Skip non-pub functions from other modules
                if !tir_func.is_pub {
                    continue;
                }
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
                    let exit_available = project.used_effects.contains(&WasiEffect::Exit);
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
                let func_id =
                    FunctionId::Free(FreeFunctionName::from_path_and_name(path, &tir_func.name));
                // Skip functions not reachable from entry point (DCE)
                if !project.is_reachable(&func_id) {
                    continue;
                }
                let mangled_name = func_id.to_string();
                drop(tir_func); // Release borrow before cloning Rc
                loaded_funcs.push((
                    path.clone(),
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
        // Format: (module_path, struct_lookup_name, tir_func, type_table, mangled_name)
        let mut loaded_methods: Vec<(
            Vec<String>,
            StructName,
            Rc<RefCell<TirFunction>>,
            Rc<RefCell<TypeTable>>,
            String,
        )> = Vec::new();
        for (path, tir_mod) in all_tir_modules {
            // Skip entry module (handled separately)
            if path == &entry_tir.path {
                continue;
            }
            // Skip wasi:* modules
            if path.first().map(|s| s == "wasi").unwrap_or(false) {
                continue;
            }
            // Methods are stored as functions with mangled names like "Point::sum"
            // (resolver adds them to functions, not impls)
            for func_rc in &tir_mod.functions {
                let func = func_rc.borrow();
                // Check if this is a method (name contains ::)
                if let Some(sep_pos) = func.name.find("::") {
                    let struct_name = &func.name[..sep_pos];
                    let method_name = &func.name[sep_pos + 2..];

                    // Skip non-pub methods
                    if !func.is_pub {
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
                    // Build function ID for DCE check: path/Struct::method
                    let method_id = FunctionId::Method(MethodName::new(
                        path.join("/"),
                        struct_name.to_string(),
                        None,
                        method_name.to_string(),
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
                        StructName::from_path_and_name(path, struct_name)
                    } else {
                        // No collision - use simple StructName (empty module path)
                        StructName::new(vec![], struct_name.to_string())
                    };
                    // Use the same fully mangled name for registration
                    // This ensures consistency between DCE tracking and codegen
                    drop(func); // Release borrow before cloning Rc
                    loaded_methods.push((
                        path.clone(),
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
        for (module_path, tir_func_rc, _, qualified_name) in &loaded_funcs {
            if !module_path.is_empty() {
                _import_lookup.insert(tir_func_rc.borrow().name.clone(), qualified_name.clone());
            }
        }

        // ========================================
        // Define types using the builder
        // ========================================

        // Builtin function types - derived from core/builtin.wado
        // DCE: Only define types for builtins that are actually used
        for func in self.builtin_registry.imported_builtins() {
            let canonical_name = func.canonical_name.as_ref().unwrap();
            // Skip if this builtin is not used
            if let Some(builtin) = CanonBuiltin::from_str(canonical_name) {
                if !project.used_builtins.contains(&builtin) {
                    continue;
                }
            } else {
                continue; // Unknown builtin, skip
            }
            let params = self.builtin_func_to_core_params(func);
            let results = self.builtin_func_to_core_results(func);
            builder.define_func_type(canonical_name, &params, &results);
        }

        // GC string array type (array<u8>) - mutable to support float-to-string conversion
        self.string_array_type_idx =
            builder.define_gc_array_type("string-array", StorageType::I8, true);

        let _string_array_idx_for_structs = builder.type_idx("string-array");

        // Register PRIMITIVE array types first (elements are primitives)
        // These don't depend on struct types
        self.register_primitive_array_types_from_table(type_table, &mut builder);
        for (path, tir_mod) in all_tir_modules {
            if path != &entry_tir.path {
                self.register_primitive_array_types_from_table(
                    &tir_mod.type_table.borrow(),
                    &mut builder,
                );
            }
        }

        // PHASE 1: Register NON-MONOMORPHIZED structs from library modules
        // These are "base" structs like String that don't depend on array types
        // - If no collision: register with simple name (empty module path)
        // - If collision with main module: register with qualified name (full module path)
        // Note: all_tir_modules is in topological order (dependency modules first)
        for (path, tir_mod) in all_tir_modules {
            // Skip entry module (handled separately)
            if path == &entry_tir.path {
                continue;
            }
            for tir_struct in &tir_mod.structs {
                if !tir_struct.is_pub {
                    continue;
                }
                // Skip generic struct templates - they will be registered when monomorphized
                if !tir_struct.type_params.is_empty() {
                    continue;
                }
                // Also skip structs that contain type parameters in field types
                // (these are generic templates that weren't properly monomorphized)
                if self.struct_contains_type_params(tir_struct, &tir_mod.type_table.borrow()) {
                    continue;
                }
                // Skip monomorphized structs for now - they need array types registered first
                if tir_struct.monomorph_info.is_some() {
                    continue;
                }
                let struct_name = if main_module_struct_names.contains(&tir_struct.name) {
                    // Collision - use qualified name with full module path
                    StructName::from_path_and_name(path, &tir_struct.name)
                } else {
                    // No collision - use simple name (empty module path)
                    StructName::new(vec![], tir_struct.name.clone())
                };
                self.register_struct_type(
                    struct_name,
                    tir_struct,
                    &tir_mod.type_table.borrow(),
                    &mut builder,
                );
            }
        }

        // PHASE 2: Register NON-MONOMORPHIZED main module structs
        // Skip generic struct templates and monomorphized structs
        // Sort structs topologically to ensure dependencies are registered before dependents
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
        let sorted_non_mono = Self::sort_structs_topologically(&non_mono_structs, type_table);
        for tir_struct in sorted_non_mono {
            let struct_name = StructName::new(vec![], tir_struct.name.clone());
            self.register_struct_type(struct_name, tir_struct, type_table, &mut builder);
        }

        // Register struct type aliases (e.g., `Point as OtherPoint`)
        for (alias_name, alias_module_path, original_name) in symbols.get_struct_aliases() {
            let alias_struct_name = StructName::new(vec![], alias_name.clone());
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
                let original_struct_name = StructName::new(vec![], original_name.clone());
                if let Some(info) = self.struct_types.get(&original_struct_name).cloned() {
                    self.struct_types.insert(alias_struct_name, info);
                }
            }
        }

        // Register tuple types from all TIR modules
        self.register_tuple_types_from_table(type_table, &mut builder);
        for (path, tir_mod) in all_tir_modules {
            if path != &entry_tir.path {
                self.register_tuple_types_from_table(&tir_mod.type_table.borrow(), &mut builder);
            }
        }

        // PHASE 3: Register MONOMORPHIZED structs from library modules
        // These must be registered BEFORE array types because array types with
        // generic struct elements (e.g., Array<Pair<i32, String>>) need to call
        // type_id_to_valtype which requires the struct to be registered.
        // Skip Array monomorphized structs - they're handled by array_struct_types.
        for (path, tir_mod) in all_tir_modules {
            if path == &entry_tir.path {
                continue;
            }
            for tir_struct in &tir_mod.structs {
                if !tir_struct.is_pub {
                    continue;
                }
                // Only register monomorphized structs in this phase
                if tir_struct.monomorph_info.is_none() {
                    continue;
                }
                // Skip Array monomorphized structs - they use array_struct_types
                if let Some(info) = &tir_struct.monomorph_info
                    && info.generic_name == "Array"
                {
                    continue;
                }
                let struct_name = StructName::new(vec![], tir_struct.name.clone());
                self.register_struct_type(
                    struct_name,
                    tir_struct,
                    &tir_mod.type_table.borrow(),
                    &mut builder,
                );
            }
        }

        // PHASE 4: Register MONOMORPHIZED main module structs
        // Skip Array monomorphized structs - they're handled by array_struct_types
        let mono_structs: Vec<_> = entry_tir
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
            .cloned()
            .collect();
        let sorted_mono = Self::sort_structs_topologically(&mono_structs, type_table);
        for tir_struct in sorted_mono {
            let struct_name = StructName::new(vec![], tir_struct.name.clone());
            self.register_struct_type(struct_name, tir_struct, type_table, &mut builder);
        }

        // PHASE 4.5: Register variant types (tagged unions)
        // Register variants from imported modules first
        for (path, tir_mod) in all_tir_modules {
            if path == &entry_tir.path {
                continue;
            }
            for variant in &tir_mod.variants {
                if !variant.is_pub {
                    continue;
                }
                // Skip generic variants - they will be registered when monomorphized
                if !variant.type_params.is_empty() {
                    continue;
                }
                self.register_variant_type(variant, &tir_mod.type_table.borrow(), &mut builder);
            }
        }

        // Register variants from main module
        for variant in &entry_tir.variants {
            // Skip generic variants - they will be registered when monomorphized
            if !variant.type_params.is_empty() {
                continue;
            }
            self.register_variant_type(variant, type_table, &mut builder);
        }

        // PHASE 5: Register ALL array types (including struct-based like Array<String>)
        // This must happen after ALL struct registration (including monomorphized ones)
        // because array types with struct elements need type_id_to_valtype to work.
        self.register_array_types_from_table(type_table, &mut builder);
        for (path, tir_mod) in all_tir_modules {
            if path != &entry_tir.path {
                self.register_array_types_from_table(&tir_mod.type_table.borrow(), &mut builder);
            }
        }

        // Register box types for primitive references (&i32, &mut f64, etc.)
        self.register_box_types(&mut builder, project);

        // Register canonical closure types for function type parameters.
        // This must happen BEFORE user-defined function types are defined,
        // because function parameters of type fn(T1, T2) -> R need to use
        // the canonical closure struct type.
        self.register_canonical_closure_types_from_table(type_table, &mut builder);
        for (path, tir_mod) in all_tir_modules {
            if path != &entry_tir.path {
                self.register_canonical_closure_types_from_table(
                    &tir_mod.type_table.borrow(),
                    &mut builder,
                );
            }
        }

        // WASI effect function types - derived from wasi/*.wado definitions
        // DCE: Only define types for WASI functions that are actually available (lowered)
        for interface in self.wasi_registry.interfaces() {
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

            // Methods have names like "Point::sum" - use fully mangled name for type
            let type_name = if let Some(sep_pos) = tir_func.name.find("::") {
                let struct_name = &tir_func.name[..sep_pos];
                let method_name = &tir_func.name[sep_pos + 2..];
                MethodName::new(
                    entry_tir.path.join("/"),
                    struct_name.to_string(),
                    None,
                    method_name.to_string(),
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
                        // Use type_id_to_valtype which knows how to look up Array types
                        // via array_struct_types registry
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
                        panic!(
                            "struct type not found for &self parameter: {} (method: {}, available: {:?})",
                            struct_name,
                            mangled_name,
                            self.struct_types.keys().collect::<Vec<_>>()
                        );
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

        // World export types - derived from Command world in wasi/cli.wado
        if let Some(run_export) = self.world_registry.get_export("Command", "run") {
            let params = self.world_export_to_core_params(run_export);
            let results = self.world_export_to_core_results(run_export);
            builder.define_func_type(&run_export.name, &params, &results);
        }

        // Collect and register closure types (types only, not functions yet)
        // Note: canonical closure types are already registered earlier, so this
        // will reuse them for closures with matching signatures.
        let collected_closures = Self::collect_closures_from_module(entry_tir);
        let mut closure_infos =
            self.register_closure_types(&collected_closures, type_table, &mut builder);

        // Add types section to module
        module.section(builder.types());

        // ========================================
        // Import section
        // ========================================
        // DCE: Only import builtins that are actually used
        for builtin in &project.used_builtins {
            let canonical_name = builtin.canonical_name();
            if let Some(info) = self.builtin_registry.get_by_canonical(canonical_name) {
                builder.import_func(&info.namespace, canonical_name);
            }
        }

        // Import lowered WASI functions
        // Only import functions that are available (lowered at component level)
        for local_name in available_wasi_funcs {
            builder.import_func("wasi", local_name);
        }

        builder.import_memory("env", "memory", 1);
        module.section(builder.imports());

        // ========================================
        // Define closure functions (after imports)
        // ========================================
        Self::define_closure_funcs(&mut closure_infos, &mut builder);
        // Store closure infos for code generation
        *self.pending_closures.borrow_mut() = closure_infos;

        // ========================================
        // Function section
        // ========================================
        // Declare all TIR functions except 'run' (which is handled as entry point)
        for tir_func_rc in &entry_tir.functions {
            let tir_func = tir_func_rc.borrow();
            if tir_func.name == "run" {
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
            // Methods have names like "Point::sum" or "Point^Trait::method" - use fully mangled name
            if let Some(sep_pos) = tir_func.name.find("::") {
                let prefix = &tir_func.name[..sep_pos];
                let method_name = &tir_func.name[sep_pos + 2..];
                // Check for trait impl: "StructName^TraitName"
                let (struct_name, trait_name) = if let Some(caret_pos) = prefix.find('^') {
                    (
                        &prefix[..caret_pos],
                        Some(prefix[caret_pos + 1..].to_string()),
                    )
                } else {
                    (prefix, None)
                };
                let mangled_name = MethodName::new(
                    entry_tir.path.join("/"),
                    struct_name.to_string(),
                    trait_name,
                    method_name.to_string(),
                )
                .to_string();
                let func_idx = builder.define_func(&mangled_name, &mangled_name);
                // For monomorphized methods, also register an alias
                // with just the simple name (e.g., Array<i32>::len)
                if tir_func.monomorph_info.is_some() {
                    builder.define_func_alias(&tir_func.name, func_idx);
                }
            } else {
                builder.define_func(&tir_func.name, &tir_func.name);
            }
        }
        // Declare loaded module functions with simple name aliases
        // This matches the AST path behavior where functions can be called by simple name
        let internal_path = vec!["core".to_string(), "internal".to_string()];
        for (module_path, tir_func_rc, _, qualified_name) in &loaded_funcs {
            let tir_func = tir_func_rc.borrow();
            let func_idx = builder.define_func(qualified_name, qualified_name);
            let is_from_internal = module_path == &internal_path;

            // Register simple name alias for all functions EXCEPT internal
            // Internal functions require explicit import to be accessible
            if qualified_name != &tir_func.name && !is_from_internal {
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
        // Declare 'run' as the entry point
        builder.define_func("run", "run");
        module.section(builder.functions());

        // ========================================
        // Export section
        // ========================================
        builder.export_func("run", "run");
        module.section(builder.exports());

        // ========================================
        // Element section (required for ref.func in closures)
        // ========================================
        let pending_closures = self.pending_closures.borrow();
        if !pending_closures.is_empty() {
            let mut elements = ElementSection::new();
            // Collect closure function indices for declarative element segment
            let closure_func_indices: Vec<u32> =
                pending_closures.iter().map(|c| c.func_idx).collect();
            // Create declarative element segment for ref.func usage
            elements.declared(Elements::Functions(std::borrow::Cow::Borrowed(
                &closure_func_indices,
            )));
            module.section(&elements);
        }
        drop(pending_closures);

        // Data count section (required for array.new_data with GC)
        let data_count = u32::from(!string_data.is_empty());
        module.section(&DataCountSection { count: data_count });

        // ========================================
        // Code section
        // ========================================
        // Reset closure codegen counter - must match the order closures were collected
        *self.closure_codegen_counter.borrow_mut() = 0;
        let mut code = CodeSection::new();
        let mut all_branch_hints: Vec<(u32, Vec<(u32, bool)>)> = Vec::new();
        let mut func_idx = builder.import_func_count;
        let empty_path: &[String] = &[];

        // Generate closure implementation function bodies FIRST (they were declared first)
        let pending_closures = self.pending_closures.borrow().clone();
        for closure_info in &pending_closures {
            let wasm_func = self.generate_closure_function(closure_info, type_table, &builder);
            code.function(&wasm_func);
            func_idx += 1;
        }

        // Generate user-defined functions from entry TIR (excluding 'run' which is handled specially)
        for tir_func_rc in &entry_tir.functions {
            let tir_func = tir_func_rc.borrow();
            if tir_func.name == "run" {
                continue; // Skip run - it's handled separately as entry point
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
            let (wasm_func, hints) =
                self.generate_function(&tir_func, type_table, &builder, empty_path);
            code.function(&wasm_func);
            if !hints.is_empty() {
                all_branch_hints.push((func_idx, hints));
            }
            func_idx += 1;
        }

        // Generate loaded module functions (TIR path)
        for (module_path, tir_func_rc, func_type_table_rc, _qualified_name) in &loaded_funcs {
            let tir_func = tir_func_rc.borrow();
            let func_type_table = &*func_type_table_rc.borrow();
            // Skip generic template functions - only generate monomorphized instances
            if (!tir_func.type_params.is_empty() || !tir_func.impl_type_params.is_empty())
                && tir_func.monomorph_info.is_none()
            {
                continue;
            }

            let (wasm_func, hints) =
                self.generate_function(&tir_func, func_type_table, &builder, module_path);
            code.function(&wasm_func);
            if !hints.is_empty() {
                all_branch_hints.push((func_idx, hints));
            }
            func_idx += 1;
        }

        // Generate impl methods from loaded modules (TIR path)
        for (module_path, _struct_name, tir_method_rc, method_type_table_rc, _mangled_name) in
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

            let (wasm_func, hints) =
                self.generate_function(&tir_method, method_type_table, &builder, module_path);
            code.function(&wasm_func);
            if !hints.is_empty() {
                all_branch_hints.push((func_idx, hints));
            }
            func_idx += 1;
        }

        // Generate run function (entry point with task.return wrapper)
        let run_tir_rc = entry_tir
            .functions
            .iter()
            .find(|f| f.borrow().name == "run");

        let run_wasm_func = if let Some(run_tir_rc) = run_tir_rc {
            // Generate run body using the TIR function body generation
            let run_tir = run_tir_rc.borrow();
            self.generate_run_function(&run_tir, type_table, &builder)
        } else {
            // No run function - create empty entry point
            let mut func = Function::new(vec![]);
            let task_return_idx = builder.func_idx("task-return");
            func.instruction(&Instruction::I32Const(0));
            func.instruction(&Instruction::Call(task_return_idx));
            func.instruction(&Instruction::End);
            func
        };

        code.function(&run_wasm_func);

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

        module.finish()
    }

    /// Generate component from TIR for WASI P3
    /// Uses native stream<T> types and imports wasi:cli/stdout
    fn generate_component(
        &mut self,
        entry_tir: &TirModule,
        all_tir_modules: &IndexMap<Vec<String>, TirModule>,
        symbols: &SymbolTable,
        _implicit_modules: &std::collections::HashSet<Vec<String>>,
        project: &Project,
        module_name: &str,
    ) -> Vec<u8> {
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
        let mem_module = self.build_memory_module(&string_data, project.strip_names);
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
        // Float-to-string conversion module (conditionally included)
        // ========================================
        if project.needs_float_to_string() {
            let fts_module =
                wasm_postprocess::convert_memory_to_import(wado_bundled_wasm(), "env", "memory")
                    .expect("Failed to process float-to-string module");
            ctx.register_core_module("fts-mod");
            builder.core_module_raw(Some("fts-mod"), &fts_module);

            // Create env instance for float-to-string (just memory)
            ctx.register_core_instance("fts-env");
            let fts_env_exports = [("memory", ExportKind::Memory, ctx.memory_idx())];
            let fts_env_instance =
                builder.core_instantiate_exports(Some("fts-env-instance"), fts_env_exports);

            // Instantiate float-to-string module with memory
            ctx.register_core_instance("fts");
            builder.core_instantiate(
                Some("fts"),
                ctx.core_module_idx("fts-mod"),
                [("env", ModuleArg::Instance(fts_env_instance))],
            );

            // Alias float-to-string exports (only the ones needed)
            if project.used_builtins.contains(&CanonBuiltin::F64ToBuffer) {
                ctx.register_core_func("f64-to-buffer");
                builder.core_alias_export(
                    Some("f64-to-buffer"),
                    ctx.core_instance_idx("fts"),
                    "f64_to_buffer",
                    ExportKind::Func,
                );
            }

            if project.used_builtins.contains(&CanonBuiltin::F32ToBuffer) {
                ctx.register_core_func("f32-to-buffer");
                builder.core_alias_export(
                    Some("f32-to-buffer"),
                    ctx.core_instance_idx("fts"),
                    "f32_to_buffer",
                    ExportKind::Func,
                );
            }
        }

        // ========================================
        // Stream canonical intrinsics for stream<u8>
        // DCE: Only generate canon functions that are actually used
        // ========================================
        if project.used_builtins.contains(&CanonBuiltin::StreamNew) {
            ctx.register_core_func("stream-new");
            builder.stream_new(stream_u8_type);
        }

        if project.used_builtins.contains(&CanonBuiltin::StreamWrite) {
            ctx.register_core_func("stream-write");
            builder.stream_write(
                stream_u8_type,
                [
                    CanonicalOption::Memory(ctx.memory_idx()),
                    CanonicalOption::Realloc(ctx.core_func_idx("realloc")),
                ],
            );
        }

        if project
            .used_builtins
            .contains(&CanonBuiltin::StreamDropWritable)
        {
            ctx.register_core_func("stream-drop-writable");
            builder.stream_drop_writable(stream_u8_type);
        }

        if project
            .used_builtins
            .contains(&CanonBuiltin::StreamDropReadable)
        {
            ctx.register_core_func("stream-drop-readable");
            builder.stream_drop_readable(stream_u8_type);
        }

        // Lower write-via-stream (stdout) - only if stdout interface is available
        let stdout_func_name = build_local_alias_name("cli", "Stdout", "write_via_stream");
        if ctx.has_comp_func(&stdout_func_name) {
            ctx.register_core_func(&stdout_func_name);
            builder.lower_func(
                Some(&stdout_func_name),
                ctx.comp_func_idx(&stdout_func_name),
                [
                    CanonicalOption::Async,
                    CanonicalOption::Memory(ctx.memory_idx()),
                    CanonicalOption::Realloc(ctx.core_func_idx("realloc")),
                ],
            );
        }

        // Lower write-via-stream (stderr) - only if stderr interface is available
        let stderr_func_name = build_local_alias_name("cli", "Stderr", "write_via_stream");
        if ctx.has_comp_func(&stderr_func_name) {
            ctx.register_core_func(&stderr_func_name);
            builder.lower_func(
                Some(&stderr_func_name),
                ctx.comp_func_idx(&stderr_func_name),
                [
                    CanonicalOption::Async,
                    CanonicalOption::Memory(ctx.memory_idx()),
                    CanonicalOption::Realloc(ctx.core_func_idx("realloc")),
                ],
            );
        }

        // Lower monotonic-clock-now component func to core func (if available)
        // This is a sync function: func() -> u64, no memory/realloc needed
        let monotonic_clock_func_name = build_local_alias_name("clocks", "MonotonicClock", "now");
        if ctx.has_comp_func(&monotonic_clock_func_name) {
            ctx.register_core_func(&monotonic_clock_func_name);
            builder.lower_func(
                Some(&monotonic_clock_func_name),
                ctx.comp_func_idx(&monotonic_clock_func_name),
                [],
            );
        }

        // Lower Environment functions (if available)
        // These return list<string> or option<string>, need memory/realloc
        let get_args_name = build_local_alias_name("cli", "Environment", "get_arguments");
        if ctx.has_comp_func(&get_args_name) {
            ctx.register_core_func(&get_args_name);
            builder.lower_func(
                Some(&get_args_name),
                ctx.comp_func_idx(&get_args_name),
                [
                    CanonicalOption::Memory(ctx.memory_idx()),
                    CanonicalOption::Realloc(ctx.core_func_idx("realloc")),
                ],
            );
        }

        let get_env_name = build_local_alias_name("cli", "Environment", "get_environment");
        if ctx.has_comp_func(&get_env_name) {
            ctx.register_core_func(&get_env_name);
            builder.lower_func(
                Some(&get_env_name),
                ctx.comp_func_idx(&get_env_name),
                [
                    CanonicalOption::Memory(ctx.memory_idx()),
                    CanonicalOption::Realloc(ctx.core_func_idx("realloc")),
                ],
            );
        }

        let get_cwd_name = build_local_alias_name("cli", "Environment", "get_initial_cwd");
        if ctx.has_comp_func(&get_cwd_name) {
            ctx.register_core_func(&get_cwd_name);
            builder.lower_func(
                Some(&get_cwd_name),
                ctx.comp_func_idx(&get_cwd_name),
                [
                    CanonicalOption::Memory(ctx.memory_idx()),
                    CanonicalOption::Realloc(ctx.core_func_idx("realloc")),
                ],
            );
        }

        // Lower Exit functions (if available)
        // exit takes result, exit-with-code takes u8
        let exit_name = build_local_alias_name("cli", "Exit", "exit");
        if ctx.has_comp_func(&exit_name) {
            ctx.register_core_func(&exit_name);
            builder.lower_func(Some(&exit_name), ctx.comp_func_idx(&exit_name), []);
        }

        let exit_code_name = build_local_alias_name("cli", "Exit", "exit_with_code");
        if ctx.has_comp_func(&exit_code_name) {
            ctx.register_core_func(&exit_code_name);
            builder.lower_func(
                Some(&exit_code_name),
                ctx.comp_func_idx(&exit_code_name),
                [],
            );
        }

        // Async intrinsics - DCE: only generate if used
        if project.used_builtins.contains(&CanonBuiltin::TaskReturn) {
            ctx.register_core_func("task-return");
            builder.task_return(Some(ComponentValType::Type(result_unit_type)), []);
        }

        if project
            .used_builtins
            .contains(&CanonBuiltin::WaitableSetNew)
        {
            ctx.register_core_func("waitable-set-new");
            builder.waitable_set_new();
        }

        if project.used_builtins.contains(&CanonBuiltin::WaitableJoin) {
            ctx.register_core_func("waitable-join");
            builder.waitable_join();
        }

        if project
            .used_builtins
            .contains(&CanonBuiltin::WaitableSetWait)
        {
            ctx.register_core_func("waitable-set-wait");
            builder.waitable_set_wait(false, ctx.memory_idx());
        }

        if project.used_builtins.contains(&CanonBuiltin::SubtaskDrop) {
            ctx.register_core_func("subtask-drop");
            builder.subtask_drop();
        }

        // ========================================
        // Collect available WASI functions (those that were lowered)
        // ========================================
        let mut available_wasi_funcs: HashSet<String> = HashSet::new();
        for interface in self.wasi_registry.interfaces() {
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

        // Add canonical builtins with namespace "wasi"
        for builtin in &project.used_builtins {
            let canonical_name = builtin.canonical_name();
            if let Some(info) = self.builtin_registry.get_by_canonical(canonical_name)
                && info.namespace == "wasi"
            {
                wasi_exports.push((
                    canonical_name.to_string(),
                    ExportKind::Func,
                    ctx.core_func_idx(canonical_name),
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
        let wasi_exports_refs: Vec<_> = wasi_exports
            .iter()
            .map(|(name, kind, idx)| (name.as_str(), *kind, *idx))
            .collect();
        let wasi_instance =
            builder.core_instantiate_exports(Some("wasi-instance"), wasi_exports_refs);
        ctx.register_core_instance("wasi");

        let mut env_exports: Vec<(&str, ExportKind, u32)> = vec![
            ("memory", ExportKind::Memory, ctx.memory_idx()),
            ("realloc", ExportKind::Func, ctx.core_func_idx("realloc")),
        ];
        if project.used_builtins.contains(&CanonBuiltin::F64ToBuffer) {
            env_exports.push((
                "f64_to_buffer",
                ExportKind::Func,
                ctx.core_func_idx("f64-to-buffer"),
            ));
        }
        if project.used_builtins.contains(&CanonBuiltin::F32ToBuffer) {
            env_exports.push((
                "f32_to_buffer",
                ExportKind::Func,
                ctx.core_func_idx("f32-to-buffer"),
            ));
        }
        let env_instance = builder.core_instantiate_exports(Some("env-instance"), env_exports);
        ctx.register_core_instance("env");

        // Instantiate main module
        ctx.register_core_instance("main");
        builder.core_instantiate(
            Some("main"),
            ctx.core_module_idx("main-mod"),
            [
                ("wasi", ModuleArg::Instance(wasi_instance)),
                ("env", ModuleArg::Instance(env_instance)),
            ],
        );

        // Alias run function from main instance
        ctx.register_core_func("run-core");
        builder.core_alias_export(
            Some("run-core"),
            ctx.core_instance_idx("main"),
            "run",
            ExportKind::Func,
        );

        // Type: async run function type () -> result
        let run_func_type = ctx.register_type("run-func-type");
        {
            let (_, enc) = builder.ty(Some("run-func-type"));
            enc.function()
                .async_(true)
                .params::<[(&str, ComponentValType); 0], ComponentValType>([])
                .result(Some(ComponentValType::Type(result_unit_type)));
        }

        // Lift run function with Async option
        ctx.register_comp_func("run");
        builder.lift_func(
            Some("run"),
            ctx.core_func_idx("run-core"),
            run_func_type,
            [
                CanonicalOption::Async,
                CanonicalOption::Memory(ctx.memory_idx()),
            ],
        );

        // Export run function
        builder.export(
            "run",
            ComponentExportKind::Func,
            ctx.comp_func_idx("run"),
            None,
        );

        // Add component-level debug names (skip in size-optimized builds)
        if !project.strip_names {
            builder.append_names();
        }

        builder.finish()
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
        let cli_version = self
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
        for interface_info in self.wasi_registry.interfaces() {
            // Skip interfaces that define exports (not imports)
            // The "run" interface defines the component's entry point export.
            // Note: "run" is needed for the wasi:cli Command world, which Wado
            // doesn't fully implement yet. When Command world support is added,
            // this should be handled as an export, not an import.
            if interface_info.interface == "run" {
                continue;
            }

            // Only include interfaces where ALL functions have supported types
            // This ensures we're requesting exactly what we can generate,
            // avoiding mismatches with runtime-provided interfaces
            let all_functions_supported = interface_info
                .functions
                .iter()
                .all(is_wasi_function_supported);

            if !all_functions_supported {
                continue;
            }

            // DCE: Skip interfaces where the effect is not used
            // Get effect name from first function (all functions in an interface share the same effect)
            if let Some(first_func) = interface_info.functions.first() {
                let effect_name = &first_func.effect_name;
                // Convert string effect name to WasiEffect
                let effect_is_used = WasiEffect::from_str(effect_name)
                    .is_some_and(|e| project.used_effects.contains(&e));
                if !effect_is_used {
                    continue;
                }
            }

            // All functions are supported, so use them all
            let supported_functions: Vec<_> = interface_info.functions.iter().collect();

            // Build instance type for this interface
            let instance_type_name = format!("{}-instance-type", interface_info.interface);
            let instance_type_idx = ctx.register_type(&instance_type_name);
            {
                let (_, enc) = builder.ty(Some(&instance_type_name));
                let mut instance_type = InstanceType::new();
                let mut local_type_idx = 0u32;

                // Track which functions need which types
                // We'll build types first, then functions
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

                    // Error-code alias (if needed for result type)
                    let error_code_idx = if needs_error_code {
                        let outer_error_code = ctx.type_idx("error-code");
                        instance_type.alias(Alias::Outer {
                            kind: ComponentOuterAliasKind::Type,
                            count: 1,
                            index: outer_error_code,
                        });
                        let idx = local_type_idx;
                        local_type_idx += 1;
                        Some(idx)
                    } else {
                        None
                    };

                    // Result type for return type (with error-code)
                    let result_type_idx = if let Some(err_idx) = error_code_idx {
                        instance_type
                            .ty()
                            .defined_type()
                            .result(None, Some(ComponentValType::Type(err_idx)));
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

                    // Build function type
                    // Build params - convert names to kebab-case for CM
                    let kebab_params: Vec<(String, ComponentValType)> = func
                        .params
                        .iter()
                        .map(|(name, ty)| {
                            let val_type = self.wado_type_to_cm_val_type(
                                ty,
                                stream_type_idx,
                                error_code_idx,
                                result_param_type_idx,
                            );
                            (to_kebab_case(name), val_type)
                        })
                        .collect();
                    // Convert to references for the encoder
                    let params: Vec<(&str, ComponentValType)> = kebab_params
                        .iter()
                        .map(|(name, val_type)| (name.as_str(), *val_type))
                        .collect();

                    // Build result
                    let result_type = func.return_type.as_ref().map(|ty| {
                        self.wado_type_to_cm_result_type(
                            ty,
                            result_type_idx,
                            array_type_idx,
                            option_type_idx,
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

                    // Export the function
                    instance_type.export(
                        &func.wasi_func_name,
                        wasm_encoder::ComponentTypeRef::Func(func_type_idx),
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
                let local_name = self
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

        // Import stdout/stderr if needed but not already imported from registry
        // (previously always imported for panic support, now DCE-aware)
        self.ensure_stdout_stderr_imported(builder, ctx, cli_version, project);

        // Import environment interface if needed
        self.ensure_environment_imported(builder, ctx, cli_version, project);

        // Import exit interface if needed
        self.ensure_exit_imported(builder, ctx, cli_version, project);
    }

    /// Ensure stdout and stderr are imported if they're used.
    ///
    /// This provides a fallback if the effect wasn't imported through the registry loop
    /// but is still needed (e.g., for panic/logging support).
    /// Uses the version from the registry.
    fn ensure_stdout_stderr_imported(
        &self,
        builder: &mut ComponentBuilder,
        ctx: &mut ComponentModelContext,
        cli_version: &str,
        project: &Project,
    ) {
        // Import stdout if used but not already imported
        let stdout_local_name = build_local_alias_name("cli", "Stdout", "write_via_stream");
        if project.used_effects.contains(&WasiEffect::Stdout)
            && !ctx.has_comp_func(&stdout_local_name)
        {
            // Try to get function info from registry for dynamic signature
            let func_info = self.wasi_registry.get_stdout_write_via_stream();
            let is_async = func_info.map(|f| f.is_async).unwrap_or(true);

            let stdout_instance_type = ctx.register_type("stdout-instance-type");
            {
                let (_, enc) = builder.ty(Some("stdout-instance-type"));
                let mut instance_type = InstanceType::new();
                // Type 0: stream<u8>
                instance_type
                    .ty()
                    .defined_type()
                    .stream(Some(ComponentValType::Primitive(PrimitiveValType::U8)));
                // Type 1: error-code alias
                let outer_error_code = ctx.type_idx("error-code");
                instance_type.alias(Alias::Outer {
                    kind: ComponentOuterAliasKind::Type,
                    count: 1,
                    index: outer_error_code,
                });
                // Type 2: result<_, error-code>
                instance_type
                    .ty()
                    .defined_type()
                    .result(None, Some(ComponentValType::Type(1)));
                // Type 3: func(stream<u8>) -> result<_, error-code>
                let mut func_encoder = instance_type.ty().function();
                if is_async {
                    func_encoder.async_(true);
                }
                func_encoder
                    .params([("data", ComponentValType::Type(0))])
                    .result(Some(ComponentValType::Type(2)));
                instance_type.export("write-via-stream", wasm_encoder::ComponentTypeRef::Func(3));
                enc.instance(&instance_type);
            }

            ctx.register_instance("stdout");
            let stdout_import_path = format!("wasi:cli/stdout@{cli_version}");
            builder.import(
                &stdout_import_path,
                wasm_encoder::ComponentTypeRef::Instance(stdout_instance_type),
            );

            ctx.register_comp_func(&stdout_local_name);
            builder.alias_export(
                ctx.instance_idx("stdout"),
                "write-via-stream",
                ComponentExportKind::Func,
            );
        }

        // Import stderr if used but not already imported
        let stderr_local_name = build_local_alias_name("cli", "Stderr", "write_via_stream");
        if project.used_effects.contains(&WasiEffect::Stderr)
            && !ctx.has_comp_func(&stderr_local_name)
        {
            // Try to get function info from registry for dynamic signature
            let func_info = self.wasi_registry.get_stderr_write_via_stream();
            let is_async = func_info.map(|f| f.is_async).unwrap_or(true);

            let stderr_instance_type = ctx.register_type("stderr-instance-type");
            {
                let (_, enc) = builder.ty(Some("stderr-instance-type"));
                let mut instance_type = InstanceType::new();
                // Type 0: stream<u8>
                instance_type
                    .ty()
                    .defined_type()
                    .stream(Some(ComponentValType::Primitive(PrimitiveValType::U8)));
                // Type 1: error-code alias
                let outer_error_code = ctx.type_idx("error-code");
                instance_type.alias(Alias::Outer {
                    kind: ComponentOuterAliasKind::Type,
                    count: 1,
                    index: outer_error_code,
                });
                // Type 2: result<_, error-code>
                instance_type
                    .ty()
                    .defined_type()
                    .result(None, Some(ComponentValType::Type(1)));
                // Type 3: func(stream<u8>) -> result<_, error-code>
                let mut func_encoder = instance_type.ty().function();
                if is_async {
                    func_encoder.async_(true);
                }
                func_encoder
                    .params([("data", ComponentValType::Type(0))])
                    .result(Some(ComponentValType::Type(2)));
                instance_type.export("write-via-stream", wasm_encoder::ComponentTypeRef::Func(3));
                enc.instance(&instance_type);
            }

            ctx.register_instance("stderr");
            let stderr_import_path = format!("wasi:cli/stderr@{cli_version}");
            builder.import(
                &stderr_import_path,
                wasm_encoder::ComponentTypeRef::Instance(stderr_instance_type),
            );

            ctx.register_comp_func(&stderr_local_name);
            builder.alias_export(
                ctx.instance_idx("stderr"),
                "write-via-stream",
                ComponentExportKind::Func,
            );
        }
    }

    /// Ensure Environment interface is imported if it's used.
    ///
    /// Environment provides:
    /// - get-arguments: `func()` -> list<string>
    /// - get-environment: `func()` -> list<tuple<string, string>>
    /// - initial-cwd: `func()` -> option<string>
    fn ensure_environment_imported(
        &self,
        builder: &mut ComponentBuilder,
        ctx: &mut ComponentModelContext,
        cli_version: &str,
        project: &Project,
    ) {
        let get_args_local = build_local_alias_name("cli", "Environment", "get_arguments");
        let get_env_local = build_local_alias_name("cli", "Environment", "get_environment");
        let get_cwd_local = build_local_alias_name("cli", "Environment", "get_initial_cwd");

        // Check if any Environment function is used
        let needs_environment = project.used_effects.contains(&WasiEffect::Environment)
            && (!ctx.has_comp_func(&get_args_local)
                || !ctx.has_comp_func(&get_env_local)
                || !ctx.has_comp_func(&get_cwd_local));

        if !needs_environment {
            return;
        }

        // Import the environment interface
        let env_instance_type = ctx.register_type("environment-instance-type");
        {
            let (_, enc) = builder.ty(Some("environment-instance-type"));
            let mut instance_type = InstanceType::new();

            // Type 0: list<string> (for get-arguments)
            instance_type
                .ty()
                .defined_type()
                .list(ComponentValType::Primitive(PrimitiveValType::String));

            // Type 1: tuple<string, string> (for get-environment entries)
            instance_type.ty().defined_type().tuple([
                ComponentValType::Primitive(PrimitiveValType::String),
                ComponentValType::Primitive(PrimitiveValType::String),
            ]);

            // Type 2: list<tuple<string, string>> (for get-environment)
            instance_type
                .ty()
                .defined_type()
                .list(ComponentValType::Type(1));

            // Type 3: option<string> (for initial-cwd)
            instance_type
                .ty()
                .defined_type()
                .option(ComponentValType::Primitive(PrimitiveValType::String));

            // Type 4: func() -> list<string> (get-arguments)
            instance_type
                .ty()
                .function()
                .result(Some(ComponentValType::Type(0)));

            // Type 5: func() -> list<tuple<string, string>> (get-environment)
            instance_type
                .ty()
                .function()
                .result(Some(ComponentValType::Type(2)));

            // Type 6: func() -> option<string> (initial-cwd)
            instance_type
                .ty()
                .function()
                .result(Some(ComponentValType::Type(3)));

            instance_type.export("get-arguments", wasm_encoder::ComponentTypeRef::Func(4));
            instance_type.export("get-environment", wasm_encoder::ComponentTypeRef::Func(5));
            instance_type.export("initial-cwd", wasm_encoder::ComponentTypeRef::Func(6));

            enc.instance(&instance_type);
        }

        ctx.register_instance("environment");
        let env_import_path = format!("wasi:cli/environment@{cli_version}");
        builder.import(
            &env_import_path,
            wasm_encoder::ComponentTypeRef::Instance(env_instance_type),
        );

        // Export get-arguments
        if !ctx.has_comp_func(&get_args_local) {
            ctx.register_comp_func(&get_args_local);
            builder.alias_export(
                ctx.instance_idx("environment"),
                "get-arguments",
                ComponentExportKind::Func,
            );
        }

        // Export get-environment
        if !ctx.has_comp_func(&get_env_local) {
            ctx.register_comp_func(&get_env_local);
            builder.alias_export(
                ctx.instance_idx("environment"),
                "get-environment",
                ComponentExportKind::Func,
            );
        }

        // Export initial-cwd
        if !ctx.has_comp_func(&get_cwd_local) {
            ctx.register_comp_func(&get_cwd_local);
            builder.alias_export(
                ctx.instance_idx("environment"),
                "initial-cwd",
                ComponentExportKind::Func,
            );
        }
    }

    fn ensure_exit_imported(
        &self,
        builder: &mut ComponentBuilder,
        ctx: &mut ComponentModelContext,
        cli_version: &str,
        project: &Project,
    ) {
        let exit_local = build_local_alias_name("cli", "Exit", "exit");
        let exit_code_local = build_local_alias_name("cli", "Exit", "exit_with_code");

        // Check if any Exit function is used
        // Note: Exit interface may not be supported by all runtimes,
        // so we only import it when explicitly called.
        let needs_exit = project.used_effects.contains(&WasiEffect::Exit)
            && (!ctx.has_comp_func(&exit_local) || !ctx.has_comp_func(&exit_code_local));

        if !needs_exit {
            return;
        }

        // Import the exit interface
        let exit_instance_type = ctx.register_type("exit-instance-type");
        {
            let (_, enc) = builder.ty(Some("exit-instance-type"));
            let mut instance_type = InstanceType::new();

            // Type 0: result (for exit status)
            instance_type.ty().defined_type().result(None, None);

            // Type 1: func(result) (exit)
            instance_type
                .ty()
                .function()
                .params([("status", ComponentValType::Type(0))]);

            // Type 2: func(u8) (exit-with-code)
            instance_type.ty().function().params([(
                "status-code",
                ComponentValType::Primitive(PrimitiveValType::U8),
            )]);

            instance_type.export("exit", wasm_encoder::ComponentTypeRef::Func(1));
            instance_type.export("exit-with-code", wasm_encoder::ComponentTypeRef::Func(2));

            enc.instance(&instance_type);
        }

        ctx.register_instance("exit");
        let exit_import_path = format!("wasi:cli/exit@{cli_version}");
        builder.import(
            &exit_import_path,
            wasm_encoder::ComponentTypeRef::Instance(exit_instance_type),
        );

        // Export exit
        if !ctx.has_comp_func(&exit_local) {
            ctx.register_comp_func(&exit_local);
            builder.alias_export(ctx.instance_idx("exit"), "exit", ComponentExportKind::Func);
        }

        // Export exit-with-code
        if !ctx.has_comp_func(&exit_code_local) {
            ctx.register_comp_func(&exit_code_local);
            builder.alias_export(
                ctx.instance_idx("exit"),
                "exit-with-code",
                ComponentExportKind::Func,
            );
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
                _ => panic!("unsupported Wado param type for CM: {}", named.name),
            },
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
                _ => panic!("unsupported generic return type for CM: {}", generic.name),
            },
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

    /// Register a custom variant type as a Wasm GC struct.
    ///
    /// Variant representation: (tag: i32, field0, field1, ...)
    /// - Tag field identifies the case (0-based index)
    /// - Payload fields are the union of all case fields
    ///
    /// For variants with heterogeneous field types, we use the largest field count
    /// and store all fields. Unused fields for smaller cases are zeroed.
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
        if self.variant_types.borrow().contains_key(&variant.name) {
            return self.variant_types.borrow()[&variant.name].struct_type_idx;
        }

        // Collect case metadata
        let cases: Vec<(String, usize)> = variant
            .cases
            .iter()
            .map(|c| (c.name.clone(), c.fields.len()))
            .collect();

        // Find the maximum number of fields across all cases
        let max_fields = variant
            .cases
            .iter()
            .map(|c| c.fields.len())
            .max()
            .unwrap_or(0);

        // Build the struct fields: tag (i32) + max_fields payload fields
        let mut fields = Vec::with_capacity(1 + max_fields);

        // Field 0: tag (discriminant)
        fields.push(FieldType {
            element_type: StorageType::Val(ValType::I32),
            mutable: false, // Tag is immutable once set
        });

        // Collect all field types from all cases to determine the payload types
        // For now, use a simple approach: if all fields at position i have the same type,
        // use that type; otherwise use eqref (GC supertype for all ref types)
        let mut payload_field_types: Vec<ValType> = Vec::with_capacity(max_fields);

        for field_idx in 0..max_fields {
            let mut field_types_at_idx: Vec<ValType> = Vec::new();

            for case in &variant.cases {
                if field_idx < case.fields.len() {
                    let wasm_type = self.type_id_to_valtype(type_table, case.fields[field_idx]);
                    field_types_at_idx.push(wasm_type);
                }
            }

            // Determine the storage type for this field position
            let (storage_type, field_type) = if field_types_at_idx.is_empty() {
                // No fields at this index (shouldn't happen if max_fields > 0)
                (StorageType::Val(ValType::I32), ValType::I32)
            } else if field_types_at_idx
                .iter()
                .all(|t| *t == field_types_at_idx[0])
            {
                // All cases have the same type at this position
                (
                    StorageType::Val(field_types_at_idx[0]),
                    field_types_at_idx[0],
                )
            } else {
                // Heterogeneous types - use eqref as a common supertype
                // This allows any ref type to be stored
                let eqref = ValType::Ref(RefType {
                    nullable: true,
                    heap_type: HeapType::Abstract {
                        shared: false,
                        ty: AbstractHeapType::Eq,
                    },
                });
                (StorageType::Val(eqref), eqref)
            };

            payload_field_types.push(field_type);
            fields.push(FieldType {
                element_type: storage_type,
                mutable: true,
            });
        }

        // Define the GC struct type
        let type_idx = builder.define_gc_struct_type(&variant.name, &fields);

        // Store in registry
        self.variant_types.borrow_mut().insert(
            variant.name.clone(),
            VariantTypeInfo {
                struct_type_idx: type_idx,
                cases,
                field_types: payload_field_types,
            },
        );

        type_idx
    }

    /// Register box types for primitive references.
    /// Box types are single-field mutable structs that wrap primitive values,
    /// enabling references to primitives (e.g., `&i32`, `&mut f64`).
    fn register_box_types(&mut self, builder: &mut CoreModuleBuilder, project: &Project) {
        use PrimitiveType::{I32, I16, I8, U32, U16, U8, Bool, Char, I64, U64, F32, F64};

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
        self.tuple_types.borrow().get(element_types).copied()
    }

    /// Get the Wasm GC type index for a struct or tuple type.
    /// Handles reference types by looking through to the inner type.
    fn get_struct_or_tuple_type_idx(&self, type_id: TypeId, type_table: &TypeTable) -> u32 {
        match type_table.get(type_id) {
            ResolvedType::Struct { name, module_path } => {
                if let Some(info) = self.lookup_struct_type(name, module_path) {
                    info.type_idx
                } else {
                    panic!("unknown struct type: {name}");
                }
            }
            ResolvedType::GenericInstance {
                name, type_args, ..
            } => {
                // Special case: Array<T> uses array_struct_types registry
                if name == "Array" && type_args.len() == 1 {
                    let element_type = type_args[0];
                    if let Some(&type_idx) = self.array_struct_types.get(&element_type) {
                        return type_idx;
                    }
                }
                // For other generic instances, use the mangled name lookup
                let mangled_name = self.mangle_type_for_struct_name(type_id, type_table);
                if let Some(info) = self.lookup_struct_type(&mangled_name, &[]) {
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
            | ResolvedType::String => true,
            ResolvedType::Tuple(elements) => !elements.is_empty(),
            ResolvedType::Option(inner) => self.needs_value_copy(*inner, type_table),
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
            ResolvedType::Struct { name, module_path } => {
                if let Some(info) = self.lookup_struct_type(name, module_path) {
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
                // For Array<String>, use the internal.wado helper function
                if matches!(type_table.get(elem_type), ResolvedType::String) {
                    let copy_func_idx = builder.func_idx("core/internal/array_copy_string");
                    func.instruction(&Instruction::Call(copy_func_idx));
                } else if let Some(&raw_array_type_idx) = self.array_types.get(&elem_type) {
                    // Get the Array struct type
                    let array_struct_type_idx = *self
                        .array_struct_types
                        .get(&elem_type)
                        .expect("Array struct type should be registered");
                    // Array is now a struct with (repr, used) fields
                    // 1. Store the source struct
                    let source_struct_local = ctx.alloc_local(
                        &format!("__copy_array_struct_source_{raw_array_type_idx}"),
                        ValType::Ref(RefType {
                            nullable: true,
                            heap_type: HeapType::Concrete(array_struct_type_idx),
                        }),
                    );
                    func.instruction(&Instruction::LocalSet(source_struct_local));

                    // 2. Get the repr field (raw array)
                    func.instruction(&Instruction::LocalGet(source_struct_local));
                    func.instruction(&Instruction::StructGet {
                        struct_type_index: array_struct_type_idx,
                        field_index: 0, // repr is field 0
                    });

                    // 3. Copy the raw array
                    self.generate_array_copy(func, raw_array_type_idx, false, ctx);

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
            ResolvedType::String => {
                // String is now a struct, delegate to struct copy
                if let Some(info) = self.lookup_struct_type("String", &string_module_path()) {
                    self.generate_struct_copy(func, info.type_idx, info.field_count, ctx);
                } else {
                    panic!("String struct not found in generate_value_copy");
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
        // Use pre-allocated temp local for the source struct reference
        let source_local = ctx.alloc_local(
            &format!("__copy_source_{type_idx}"),
            ValType::Ref(RefType {
                nullable: true,
                heap_type: HeapType::Concrete(type_idx),
            }),
        );

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
        // Use pre-allocated temp locals for the array copy
        let source_local = ctx.alloc_local(
            &format!("__copy_array_source_{array_type_idx}"),
            ValType::Ref(RefType {
                nullable: true,
                heap_type: HeapType::Concrete(array_type_idx),
            }),
        );
        let counter_local = ctx.alloc_local(
            &format!("__copy_array_counter_{array_type_idx}"),
            ValType::I32,
        );
        let dest_local = ctx.alloc_local(
            &format!("__copy_array_dest_{array_type_idx}"),
            ValType::Ref(RefType {
                nullable: true,
                heap_type: HeapType::Concrete(array_type_idx),
            }),
        );
        let len_local = ctx.alloc_local(
            &format!("__copy_array_len_{array_type_idx}"),
            ValType::I32,
        );

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
        let option_valtype = match inner_valtype {
            ValType::Ref(ref_type) => ValType::Ref(RefType {
                nullable: true,
                ..ref_type
            }),
            _ => {
                // For primitive inner types, option is boxed - but primitives don't need copying
                // This shouldn't happen since we check needs_value_copy first
                return;
            }
        };

        // Allocate temp local
        let source_local = ctx.alloc_local("__copy_option_source", option_valtype);

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

    /// Pre-register all tuple types found in a `TypeTable`.
    /// This must be called before code generation to ensure tuple types are available.
    fn register_tuple_types_from_table(
        &mut self,
        type_table: &TypeTable,
        builder: &mut CoreModuleBuilder,
    ) {
        for type_id in 0..type_table.len() as TypeId {
            if let ResolvedType::Tuple(elements) = type_table.get(type_id)
                && !elements.is_empty()
                && !self.tuple_types.borrow().contains_key(elements)
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
                self.tuple_types
                    .borrow_mut()
                    .insert(elements.clone(), type_idx);
            }
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
        // Check if already registered
        if let Some(&type_idx) = self.array_types.get(&element_type_id) {
            return type_idx;
        }

        // Special case: u8 arrays use the existing string_array_type_idx
        if element_type_id == TypeTable::U8 {
            return self.string_array_type_idx;
        }

        // Create new array type
        let wasm_type = self.type_id_to_valtype(type_table, element_type_id);
        let storage_type = match wasm_type {
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
        };

        // Generate a type name based on element type
        let type_name = format!("array_{element_type_id}");
        let type_idx = builder.define_gc_array_type(&type_name, storage_type, true);

        self.array_types.insert(element_type_id, type_idx);
        type_idx
    }

    /// Get or create an Array<T> struct type for a given element `TypeId`.
    /// Array<T> is a struct with fields: repr (ref to GC array), used (i32)
    /// Returns the Wasm type index for the GC struct type.
    fn get_or_create_array_struct_type(
        &mut self,
        element_type_id: TypeId,
        type_table: &TypeTable,
        builder: &mut CoreModuleBuilder,
    ) -> u32 {
        // Check if already registered
        if let Some(&type_idx) = self.array_struct_types.get(&element_type_id) {
            return type_idx;
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

        let type_name = format!("Array_{element_type_id}");
        let type_idx = builder.define_gc_struct_type(&type_name, &fields);

        self.array_struct_types.insert(element_type_id, type_idx);
        type_idx
    }

    /// Get the next unique closure ID
    fn get_next_closure_id(&self) -> u32 {
        let mut counter = self.closure_counter.borrow_mut();
        let id = *counter;
        *counter += 1;
        id
    }

    /// Get or create a closure environment type for the given captures.
    /// Returns (`env_type_idx`, `env_type_name`).
    fn get_or_create_closure_env_type(
        &self,
        captures: &[crate::tir::TirCapture],
        type_table: &TypeTable,
        builder: &mut CoreModuleBuilder,
    ) -> (u32, String) {
        // Build the key from captures
        let key: Vec<(TypeId, bool)> = captures.iter().map(|c| (c.type_id, c.is_mut)).collect();

        // Check if already registered
        if let Some(result) = self.closure_env_types.borrow().get(&key) {
            return result.clone();
        }

        // Create the environment struct type
        let closure_id = self.get_next_closure_id();
        let type_name = format!("ClosureEnv_{closure_id}");

        let fields: Vec<FieldType> = captures
            .iter()
            .map(|cap| {
                let val_type = self.type_id_to_valtype(type_table, cap.type_id);
                FieldType {
                    element_type: StorageType::Val(val_type),
                    mutable: cap.is_mut,
                }
            })
            .collect();

        let type_idx = builder.define_gc_struct_type(&type_name, &fields);

        self.closure_env_types
            .borrow_mut()
            .insert(key, (type_idx, type_name.clone()));

        (type_idx, type_name)
    }

    /// Get or create a canonical closure type for a function signature.
    /// This is used for function type parameters (e.g., `fn(i32) -> i32`).
    /// Returns (`canonical_fn_type_idx`, `canonical_fn_type_name`, `canonical_closure_struct_type_idx`).
    ///
    /// The canonical closure uses `(ref struct)` as the environment type,
    /// allowing any closure with the same user-visible signature to be compatible.
    fn get_or_create_canonical_closure_type(
        &self,
        params: &[TypeId],
        return_type: TypeId,
        type_table: &TypeTable,
        builder: &mut CoreModuleBuilder,
    ) -> (u32, String, u32) {
        let key = (params.to_vec(), return_type);

        // Check if already registered
        if let Some((fn_type_idx, fn_type_name, struct_type_idx)) =
            self.canonical_closure_types.borrow().get(&key).cloned()
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
            .borrow_mut()
            .insert(key, (fn_type_idx, fn_type_name.clone(), struct_type_idx));

        (fn_type_idx, fn_type_name, struct_type_idx)
    }

    /// Collect all closures from a TIR module.
    /// Returns a list of collected closures that need type registration.
    fn collect_closures_from_module(module: &TirModule) -> Vec<CollectedClosure> {
        let mut closures = Vec::new();

        for func_rc in &module.functions {
            let func = func_rc.borrow();
            if let Some(body) = &func.body {
                Self::collect_closures_from_block(body, &mut closures);
            }
        }

        closures
    }

    /// Collect closures from a TIR block
    fn collect_closures_from_block(block: &TirBlock, closures: &mut Vec<CollectedClosure>) {
        for stmt in &block.stmts {
            Self::collect_closures_from_stmt(stmt, closures);
        }
    }

    /// Collect closures from a TIR statement
    fn collect_closures_from_stmt(stmt: &TirStmt, closures: &mut Vec<CollectedClosure>) {
        match &stmt.kind {
            TirStmtKind::Let { value, .. } | TirStmtKind::Expr(value) => {
                Self::collect_closures_from_expr(value, closures);
            }
            TirStmtKind::While { condition, body } => {
                Self::collect_closures_from_expr(condition, closures);
                Self::collect_closures_from_block(body, closures);
            }
            TirStmtKind::For {
                condition,
                update,
                body,
            } => {
                if let Some(cond) = condition {
                    Self::collect_closures_from_expr(cond, closures);
                }
                if let Some(upd) = update {
                    Self::collect_closures_from_expr(upd, closures);
                }
                Self::collect_closures_from_block(body, closures);
            }
            TirStmtKind::ForOf { iterable, body, .. } => {
                Self::collect_closures_from_expr(iterable, closures);
                Self::collect_closures_from_block(body, closures);
            }
            TirStmtKind::Loop { body } => {
                Self::collect_closures_from_block(body, closures);
            }
            TirStmtKind::Return { value: Some(expr) } => {
                Self::collect_closures_from_expr(expr, closures);
            }
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
                ..
            } => {
                Self::collect_closures_from_expr(condition, closures);
                Self::collect_closures_from_block(then_block, closures);
                if let Some(else_blk) = else_block {
                    Self::collect_closures_from_block(else_blk, closures);
                }
            }
            TirStmtKind::LabeledBlock { block, .. } => {
                Self::collect_closures_from_block(block, closures);
            }
            TirStmtKind::IfPattern {
                scrutinee,
                then_block,
                else_block,
                ..
            } => {
                Self::collect_closures_from_expr(scrutinee, closures);
                Self::collect_closures_from_block(then_block, closures);
                if let Some(else_blk) = else_block {
                    Self::collect_closures_from_block(else_blk, closures);
                }
            }
            TirStmtKind::Return { value: None } | TirStmtKind::Break | TirStmtKind::Continue => {}
        }
    }

    /// Collect closures from a TIR expression
    fn collect_closures_from_expr(expr: &TirExpr, closures: &mut Vec<CollectedClosure>) {
        match &expr.kind {
            TirExprKind::Closure {
                params,
                body,
                captures,
            } => {
                // Determine return type:
                // - For block bodies, check for return statements
                // - Fall back to the body expression's type
                let return_type = if let TirExprKind::Block(ref block) = body.kind {
                    Self::find_return_type_in_closure_block(block).unwrap_or(body.type_id)
                } else {
                    body.type_id
                };

                // Collect this closure
                closures.push(CollectedClosure {
                    captures: captures.clone(),
                    params: params.clone(),
                    body: (**body).clone(),
                    return_type,
                });
                // Also collect any nested closures in the body
                Self::collect_closures_from_expr(body, closures);
            }
            TirExprKind::Binary { left, right, .. } => {
                Self::collect_closures_from_expr(left, closures);
                Self::collect_closures_from_expr(right, closures);
            }
            TirExprKind::Unary { expr: inner, .. }
            | TirExprKind::Cast { expr: inner, .. }
            | TirExprKind::FieldAccess { expr: inner, .. } => {
                Self::collect_closures_from_expr(inner, closures);
            }
            TirExprKind::Call { args, .. }
            | TirExprKind::EffectCall { args, .. }
            | TirExprKind::StaticCall { args, .. } => {
                for arg in args {
                    Self::collect_closures_from_expr(arg, closures);
                }
            }
            TirExprKind::MethodCall { receiver, args, .. } => {
                Self::collect_closures_from_expr(receiver, closures);
                for arg in args {
                    Self::collect_closures_from_expr(arg, closures);
                }
            }
            TirExprKind::Block(block) => {
                Self::collect_closures_from_block(block, closures);
            }
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                Self::collect_closures_from_expr(condition, closures);
                Self::collect_closures_from_block(then_branch, closures);
                if let Some(else_blk) = else_branch {
                    Self::collect_closures_from_block(else_blk, closures);
                }
            }
            TirExprKind::Assign { target, value } => {
                Self::collect_closures_from_expr(target, closures);
                Self::collect_closures_from_expr(value, closures);
            }
            TirExprKind::Index { expr: array, index } => {
                Self::collect_closures_from_expr(array, closures);
                Self::collect_closures_from_expr(index, closures);
            }
            TirExprKind::Match {
                expr: scrutinee,
                arms,
            } => {
                Self::collect_closures_from_expr(scrutinee, closures);
                for arm in arms {
                    Self::collect_closures_from_expr(&arm.body, closures);
                }
            }
            TirExprKind::StructLiteral { fields, .. } => {
                for field in fields {
                    Self::collect_closures_from_expr(&field.value, closures);
                }
            }
            TirExprKind::ArrayLiteral { elements } | TirExprKind::TupleLiteral { elements } => {
                for elem in elements {
                    Self::collect_closures_from_expr(elem, closures);
                }
            }
            TirExprKind::IndirectCall { callee, args } => {
                Self::collect_closures_from_expr(callee, closures);
                for arg in args {
                    Self::collect_closures_from_expr(arg, closures);
                }
            }
            TirExprKind::OptionSome { value } => {
                Self::collect_closures_from_expr(value, closures);
            }
            TirExprKind::VariantConstruct { fields, .. } => {
                for field in fields {
                    Self::collect_closures_from_expr(field, closures);
                }
            }
            TirExprKind::Move { value } => {
                Self::collect_closures_from_expr(value, closures);
            }
            // Leaf nodes - no nested expressions
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

    /// Find return type in a closure block body by scanning for return statements.
    /// Similar to `Resolver::find_return_type_in_block`.
    fn find_return_type_in_closure_block(block: &TirBlock) -> Option<TypeId> {
        for stmt in &block.stmts {
            if let Some(type_id) = Self::find_return_type_in_closure_stmt(stmt) {
                return Some(type_id);
            }
        }
        None
    }

    /// Find return type in a statement by scanning for return statements.
    fn find_return_type_in_closure_stmt(stmt: &TirStmt) -> Option<TypeId> {
        match &stmt.kind {
            TirStmtKind::Return { value: Some(expr) } => Some(expr.type_id),
            TirStmtKind::Return { value: None } => Some(TypeTable::UNIT),
            TirStmtKind::If {
                then_block,
                else_block,
                ..
            } => {
                // Check then branch first
                if let Some(type_id) = Self::find_return_type_in_closure_block(then_block) {
                    return Some(type_id);
                }
                // Check else branch if present
                if let Some(else_blk) = else_block
                    && let Some(type_id) = Self::find_return_type_in_closure_block(else_blk)
                {
                    return Some(type_id);
                }
                None
            }
            TirStmtKind::While { body, .. }
            | TirStmtKind::For { body, .. }
            | TirStmtKind::ForOf { body, .. }
            | TirStmtKind::Loop { body } => Self::find_return_type_in_closure_block(body),
            _ => None,
        }
    }

    /// Find local variables that store closure values.
    /// Returns a map from `local_index` to `closure_id`.
    /// This must traverse in the same order as `collect_closures_from`_* to match closure IDs.
    fn find_closure_locals(block: &TirBlock) -> HashMap<u32, u32> {
        let mut result = HashMap::new();
        let mut closure_counter: u32 = 0;
        Self::find_closure_locals_in_block(block, &mut result, &mut closure_counter);
        result
    }

    fn find_closure_locals_in_block(
        block: &TirBlock,
        result: &mut HashMap<u32, u32>,
        closure_counter: &mut u32,
    ) {
        for stmt in &block.stmts {
            Self::find_closure_locals_in_stmt(stmt, result, closure_counter);
        }
    }

    fn find_closure_locals_in_stmt(
        stmt: &TirStmt,
        result: &mut HashMap<u32, u32>,
        closure_counter: &mut u32,
    ) {
        match &stmt.kind {
            TirStmtKind::Let {
                local_index, value, ..
            } => {
                // Check if value is a closure - if so, record the mapping
                if matches!(value.kind, TirExprKind::Closure { .. }) {
                    result.insert(*local_index, *closure_counter);
                }
                // Always traverse to count all closures (in same order as collect_closures_from_expr)
                Self::find_closure_locals_in_expr(value, result, closure_counter);
            }
            TirStmtKind::Expr(value) => {
                Self::find_closure_locals_in_expr(value, result, closure_counter);
            }
            TirStmtKind::While { condition, body } => {
                Self::find_closure_locals_in_expr(condition, result, closure_counter);
                Self::find_closure_locals_in_block(body, result, closure_counter);
            }
            TirStmtKind::For {
                condition,
                update,
                body,
            } => {
                if let Some(cond) = condition {
                    Self::find_closure_locals_in_expr(cond, result, closure_counter);
                }
                if let Some(upd) = update {
                    Self::find_closure_locals_in_expr(upd, result, closure_counter);
                }
                Self::find_closure_locals_in_block(body, result, closure_counter);
            }
            TirStmtKind::ForOf { iterable, body, .. } => {
                Self::find_closure_locals_in_expr(iterable, result, closure_counter);
                Self::find_closure_locals_in_block(body, result, closure_counter);
            }
            TirStmtKind::Loop { body } => {
                Self::find_closure_locals_in_block(body, result, closure_counter);
            }
            TirStmtKind::Return { value: Some(expr) } => {
                Self::find_closure_locals_in_expr(expr, result, closure_counter);
            }
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
                ..
            } => {
                Self::find_closure_locals_in_expr(condition, result, closure_counter);
                Self::find_closure_locals_in_block(then_block, result, closure_counter);
                if let Some(else_blk) = else_block {
                    Self::find_closure_locals_in_block(else_blk, result, closure_counter);
                }
            }
            TirStmtKind::LabeledBlock { block, .. } => {
                Self::find_closure_locals_in_block(block, result, closure_counter);
            }
            TirStmtKind::IfPattern {
                scrutinee,
                then_block,
                else_block,
                ..
            } => {
                Self::find_closure_locals_in_expr(scrutinee, result, closure_counter);
                Self::find_closure_locals_in_block(then_block, result, closure_counter);
                if let Some(else_blk) = else_block {
                    Self::find_closure_locals_in_block(else_blk, result, closure_counter);
                }
            }
            TirStmtKind::Return { value: None } | TirStmtKind::Break | TirStmtKind::Continue => {}
        }
    }

    fn find_closure_locals_in_expr(
        expr: &TirExpr,
        result: &mut HashMap<u32, u32>,
        closure_counter: &mut u32,
    ) {
        match &expr.kind {
            TirExprKind::Closure { body, .. } => {
                // Count this closure (but don't record it - that's done in Let handling)
                *closure_counter += 1;
                // Also check for nested closures in the body
                Self::find_closure_locals_in_expr(body, result, closure_counter);
            }
            TirExprKind::Binary { left, right, .. } => {
                Self::find_closure_locals_in_expr(left, result, closure_counter);
                Self::find_closure_locals_in_expr(right, result, closure_counter);
            }
            TirExprKind::Unary { expr: inner, .. }
            | TirExprKind::Cast { expr: inner, .. }
            | TirExprKind::FieldAccess { expr: inner, .. } => {
                Self::find_closure_locals_in_expr(inner, result, closure_counter);
            }
            TirExprKind::Call { args, .. }
            | TirExprKind::EffectCall { args, .. }
            | TirExprKind::StaticCall { args, .. } => {
                for arg in args {
                    Self::find_closure_locals_in_expr(arg, result, closure_counter);
                }
            }
            TirExprKind::MethodCall { receiver, args, .. } => {
                Self::find_closure_locals_in_expr(receiver, result, closure_counter);
                for arg in args {
                    Self::find_closure_locals_in_expr(arg, result, closure_counter);
                }
            }
            TirExprKind::Block(block) => {
                Self::find_closure_locals_in_block(block, result, closure_counter);
            }
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                Self::find_closure_locals_in_expr(condition, result, closure_counter);
                Self::find_closure_locals_in_block(then_branch, result, closure_counter);
                if let Some(else_blk) = else_branch {
                    Self::find_closure_locals_in_block(else_blk, result, closure_counter);
                }
            }
            TirExprKind::StructLiteral { fields, .. } => {
                for field in fields {
                    Self::find_closure_locals_in_expr(&field.value, result, closure_counter);
                }
            }
            TirExprKind::TupleLiteral { elements } | TirExprKind::ArrayLiteral { elements, .. } => {
                for elem in elements {
                    Self::find_closure_locals_in_expr(elem, result, closure_counter);
                }
            }
            TirExprKind::Index { expr, index, .. } => {
                Self::find_closure_locals_in_expr(expr, result, closure_counter);
                Self::find_closure_locals_in_expr(index, result, closure_counter);
            }
            TirExprKind::Assign { target, value } => {
                Self::find_closure_locals_in_expr(target, result, closure_counter);
                Self::find_closure_locals_in_expr(value, result, closure_counter);
            }
            TirExprKind::Match { expr, arms } => {
                Self::find_closure_locals_in_expr(expr, result, closure_counter);
                for arm in arms {
                    Self::find_closure_locals_in_expr(&arm.body, result, closure_counter);
                }
            }
            TirExprKind::IndirectCall { callee, args } => {
                Self::find_closure_locals_in_expr(callee, result, closure_counter);
                for arg in args {
                    Self::find_closure_locals_in_expr(arg, result, closure_counter);
                }
            }
            TirExprKind::OptionSome { value } => {
                Self::find_closure_locals_in_expr(value, result, closure_counter);
            }
            TirExprKind::VariantConstruct { fields, .. } => {
                for field in fields {
                    Self::find_closure_locals_in_expr(field, result, closure_counter);
                }
            }
            TirExprKind::Move { value } => {
                Self::find_closure_locals_in_expr(value, result, closure_counter);
            }
            // Terminals - no closures inside
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

    /// Register types for collected closures.
    /// Called during the type definition phase before code generation.
    /// Register closure types only (env structs, function types, closure structs).
    /// Does NOT define the closure functions yet - that happens after imports.
    /// Returns partial `ClosureInfo` with `func_idx` set to 0 (placeholder).
    fn register_closure_types(
        &self,
        closures: &[CollectedClosure],
        type_table: &TypeTable,
        builder: &mut CoreModuleBuilder,
    ) -> Vec<ClosureInfo> {
        let mut closure_infos = Vec::new();

        for (idx, collected) in closures.iter().enumerate() {
            let closure_id = idx as u32;
            let func_name = format!("$closure_{closure_id}");

            // Create environment type (specific to this closure's captures)
            let (env_type_idx, _env_type_name) =
                self.get_or_create_closure_env_type(&collected.captures, type_table, builder);

            // Get or create canonical closure type for this signature.
            // The function type uses generic (ref struct) for env, allowing all closures
            // with the same user-visible signature to use the same types.
            let param_type_ids: Vec<TypeId> =
                collected.params.iter().map(|(_, tid)| *tid).collect();
            let (canonical_fn_type_idx, canonical_fn_type_name, canonical_closure_struct_type_idx) =
                self.get_or_create_canonical_closure_type(
                    &param_type_ids,
                    collected.return_type,
                    type_table,
                    builder,
                );

            // Store info - func_idx will be set later
            closure_infos.push(ClosureInfo {
                id: closure_id,
                captures: collected.captures.clone(),
                params: collected.params.clone(),
                body: collected.body.clone(),
                return_type: collected.return_type,
                env_type_idx, // Specific env type for this closure (for ref.cast inside)
                fn_type_idx: canonical_fn_type_idx, // Canonical fn type with generic env
                fn_type_name: canonical_fn_type_name, // Canonical fn type name for define_func
                closure_struct_type_idx: canonical_closure_struct_type_idx, // Canonical struct
                func_idx: 0,  // Placeholder - set in define_closure_funcs
                func_name,
            });
        }

        closure_infos
    }

    /// Define closure functions (must be called after imports are defined).
    /// Updates the `func_idx` in each `ClosureInfo`.
    fn define_closure_funcs(closure_infos: &mut [ClosureInfo], builder: &mut CoreModuleBuilder) {
        for info in closure_infos.iter_mut() {
            let func_idx = builder.define_func(&info.func_name, &info.fn_type_name);
            info.func_idx = func_idx;
        }
    }

    /// Pre-register primitive array types (where element type is a primitive).
    /// These can be registered before struct types since they don't depend on struct definitions.
    fn register_primitive_array_types_from_table(
        &mut self,
        type_table: &TypeTable,
        builder: &mut CoreModuleBuilder,
    ) {
        for type_id in 0..type_table.len() as TypeId {
            let (element_type_id, is_array_struct) =
                if let Some(elem) = type_table.as_array(type_id) {
                    (elem, true)
                } else if let ResolvedType::BuiltinArray(elem) = type_table.get(type_id) {
                    (*elem, false)
                } else {
                    continue;
                };
            // Skip if element type is not a primitive
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
            if is_array_struct && !self.array_struct_types.contains_key(&element_type_id) {
                self.get_or_create_array_struct_type(element_type_id, type_table, builder);
            }
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
        for type_id in 0..type_table.len() as TypeId {
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
            if is_array_struct && !self.array_struct_types.contains_key(&element_type_id) {
                self.get_or_create_array_struct_type(element_type_id, type_table, builder);
            }
        }
    }

    /// Pre-register canonical closure types for all function types found in the type table.
    /// This is needed so that function type parameters can be properly typed.
    fn register_canonical_closure_types_from_table(
        &self,
        type_table: &TypeTable,
        builder: &mut CoreModuleBuilder,
    ) {
        for type_id in 0..type_table.len() as TypeId {
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

            // String type - now a struct, delegate to struct lookup
            ResolvedType::String => {
                if let Some(struct_info) = self.lookup_struct_type("String", &string_module_path())
                {
                    ValType::Ref(RefType {
                        nullable: false,
                        heap_type: HeapType::Concrete(struct_info.type_idx),
                    })
                } else {
                    panic!("String struct not found in type_id_to_valtype")
                }
            }

            // Struct type
            ResolvedType::Struct { name, module_path } => {
                if let Some(struct_info) = self.lookup_struct_type(name, module_path) {
                    ValType::Ref(RefType {
                        nullable: false,
                        heap_type: HeapType::Concrete(struct_info.type_idx),
                    })
                } else {
                    panic!("unknown struct type in type_id_to_valtype: {name}")
                }
            }

            // Array<T> - GC struct with repr (raw array) and used (i32) fields
            ResolvedType::GenericInstance {
                name, type_args, ..
            } if name == "Array" && type_args.len() == 1 => {
                let element_type = type_args[0];
                // Look up registered Array struct type
                if let Some(&type_idx) = self.array_struct_types.get(&element_type) {
                    ValType::Ref(RefType {
                        nullable: false,
                        heap_type: HeapType::Concrete(type_idx),
                    })
                } else {
                    // Debug: print type_table info to understand the issue
                    let element_resolved = type_table.get(element_type);
                    panic!(
                        "Array struct type not registered for element type {} (resolved: {:?})\nAvailable array types: {:?}",
                        element_type,
                        element_resolved,
                        self.array_struct_types.keys().collect::<Vec<_>>()
                    );
                }
            }

            // builtin::array<T> - raw GC array intrinsic
            // Note: Must be nullable to match Wasm GC subtyping rules when used in struct fields
            ResolvedType::BuiltinArray(element_type) => {
                // Same as Array<T> - look up registered array type
                let type_idx = self
                    .array_types
                    .get(element_type)
                    .copied()
                    .unwrap_or(self.string_array_type_idx);
                ValType::Ref(RefType {
                    nullable: true,
                    heap_type: HeapType::Concrete(type_idx),
                })
            }

            // Option<T> - nullable reference
            ResolvedType::Option(inner) => {
                let inner_valtype = self.type_id_to_valtype(type_table, *inner);
                match inner_valtype {
                    ValType::Ref(ref_type) => ValType::Ref(RefType {
                        nullable: true,
                        ..ref_type
                    }),
                    // Primitives use i32 with sentinel value for None
                    _ => ValType::I32,
                }
            }

            // Reference types
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
                // For primitive references, use the box type
                if let ResolvedType::Primitive(prim) = type_table.get(*inner) {
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
                    self.canonical_closure_types.borrow().get(&key).cloned()
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
                } else if let Some(&type_idx) = self.tuple_types.borrow().get(elements) {
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
                // Custom variant types are represented as GC struct references
                let variant_types = self.variant_types.borrow();
                if let Some(info) = variant_types.get(name) {
                    ValType::Ref(RefType {
                        nullable: true,
                        heap_type: HeapType::Concrete(info.struct_type_idx),
                    })
                } else {
                    panic!("Variant type not registered: {name}");
                }
            }
            ResolvedType::Dict { .. } => {
                panic!("Dict type codegen not yet implemented")
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
            // Look up the monomorphized struct type
            ResolvedType::GenericInstance { .. } => {
                let mangled_name = self.mangle_type_for_struct_name(type_id, type_table);
                if let Some(struct_info) = self.lookup_struct_type(&mangled_name, &[]) {
                    ValType::Ref(RefType {
                        nullable: false,
                        heap_type: HeapType::Concrete(struct_info.type_idx),
                    })
                } else {
                    panic!(
                        "unknown monomorphized generic struct type in type_id_to_valtype: {mangled_name}"
                    )
                }
            }
        }
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
                // Reinterpret u64 bits as i64 for Wasm instruction
                match type_table.get(expr.type_id) {
                    ResolvedType::Primitive(PrimitiveType::I64 | PrimitiveType::U64) => {
                        func.instruction(&Instruction::I64Const(*value as i64));
                    }
                    _ => {
                        func.instruction(&Instruction::I32Const(*value as i32));
                    }
                }
            }

            TirExprKind::FloatLiteral { value, .. } => match type_table.get(expr.type_id) {
                ResolvedType::Primitive(PrimitiveType::F32) => {
                    func.instruction(&Instruction::F32Const(((*value) as f32).into()));
                }
                _ => {
                    func.instruction(&Instruction::F64Const((*value).into()));
                }
            },

            TirExprKind::BoolLiteral(b) => {
                func.instruction(&Instruction::I32Const(i32::from(*b)));
            }

            TirExprKind::CharLiteral(c) => {
                func.instruction(&Instruction::I32Const(*c as i32));
            }

            TirExprKind::StringLiteral(s) => {
                // String is a struct with one field: repr (builtin::array<u8>)
                // 1. Create the raw byte array
                let len = s.len();

                if len == 0 {
                    // Empty string - create empty array without data section reference
                    func.instruction(&Instruction::I32Const(0)); // length
                    func.instruction(&Instruction::ArrayNewDefault(self.string_array_type_idx));
                } else {
                    // Non-empty string - reference data section
                    let offset = self.get_string_offset(s);
                    func.instruction(&Instruction::I32Const(offset as i32));
                    func.instruction(&Instruction::I32Const(len as i32));
                    func.instruction(&Instruction::ArrayNewData {
                        array_type_index: self.string_array_type_idx,
                        array_data_index: 0,
                    });
                }

                // 2. Create the String struct
                let string_struct_info = self
                    .lookup_struct_type("String", &string_module_path())
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
                case_name: _,
                fields,
            } => {
                // Custom variant construction: Shape::Circle(5.0)
                // Layout: struct { tag: i32, field0, field1, ... }

                // Get the variant name from the type
                let variant_name = match type_table.get(*variant_type) {
                    ResolvedType::Variant { name, .. } => name.clone(),
                    other => panic!(
                        "Expected Variant type for VariantConstruct, got: {other:?}"
                    ),
                };

                // Look up the registered variant type
                let variant_types = self.variant_types.borrow();
                let variant_info = variant_types.get(&variant_name).unwrap_or_else(|| {
                    panic!("Variant type not registered: {variant_name}");
                });

                let struct_type_idx = variant_info.struct_type_idx;
                let field_types = variant_info.field_types.clone();
                let max_fields = field_types.len();
                drop(variant_types);

                // Push the tag (case index)
                func.instruction(&Instruction::I32Const(*case_index as i32));

                // Push the field values
                for field_expr in fields {
                    self.generate_expr(func, field_expr, type_table, ctx, builder);
                }

                // Pad with default values if this case has fewer fields than max
                for pad_idx in fields.len()..max_fields {
                    let field_type = &field_types[pad_idx];
                    // Generate default value for this type
                    match field_type {
                        ValType::I32 => func.instruction(&Instruction::I32Const(0)),
                        ValType::I64 => func.instruction(&Instruction::I64Const(0)),
                        ValType::F32 => func.instruction(&Instruction::F32Const(0.0_f32.into())),
                        ValType::F64 => func.instruction(&Instruction::F64Const(0.0_f64.into())),
                        ValType::Ref(_) => {
                            // For reference types, use ref.null with the appropriate heap type
                            func.instruction(&Instruction::RefNull(HeapType::Abstract {
                                shared: false,
                                ty: AbstractHeapType::None,
                            }))
                        }
                        ValType::V128 => {
                            // V128 zero constant
                            func.instruction(&Instruction::V128Const(0))
                        }
                    };
                }

                // Create the struct
                func.instruction(&Instruction::StructNew(struct_type_idx));
            }

            TirExprKind::Move { value } => {
                // Move semantics: generate the inner value without copying
                // The value is moved directly, no value copy is generated
                self.generate_expr(func, value, type_table, ctx, builder);
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

            TirExprKind::Global { module_path, name } => {
                // TODO: Handle global references properly
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

            // === Binary Operations ===
            TirExprKind::Binary { left, op, right } => {
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
                        enum ArrayKind {
                            Array { struct_type_idx: u32 },
                            String,
                        }
                        let (raw_array_type_idx, array_kind) =
                            if let Some(element_type) = type_table.as_array(base_type_id) {
                                let raw_type_idx = self
                                    .array_types
                                    .get(&element_type)
                                    .copied()
                                    .unwrap_or(self.string_array_type_idx);
                                let struct_type_idx = *self
                                    .array_struct_types
                                    .get(&element_type)
                                    .expect("Array struct type should be registered");
                                (raw_type_idx, ArrayKind::Array { struct_type_idx })
                            } else if let ResolvedType::String = base_type {
                                (self.string_array_type_idx, ArrayKind::String)
                            } else {
                                panic!("index assignment on non-array type: {base_type:?}");
                            };

                        // Generate array reference first
                        self.generate_expr(func, array_expr, type_table, ctx, builder);
                        // Access the repr field to get the raw array
                        match &array_kind {
                            ArrayKind::Array { struct_type_idx } => {
                                func.instruction(&Instruction::StructGet {
                                    struct_type_index: *struct_type_idx,
                                    field_index: 0, // repr is field 0
                                });
                            }
                            ArrayKind::String => {
                                if let Some(struct_info) =
                                    self.lookup_struct_type("String", &string_module_path())
                                {
                                    func.instruction(&Instruction::StructGet {
                                        struct_type_index: struct_info.type_idx,
                                        field_index: 0, // repr is field 0
                                    });
                                }
                            }
                        }
                        // Then generate index
                        self.generate_expr(func, index_expr, type_table, ctx, builder);
                        // Then generate value
                        self.generate_expr(func, value, type_table, ctx, builder);
                        // Emit array.set (consumes all three values, leaves nothing)
                        func.instruction(&Instruction::ArraySet(raw_array_type_idx));
                        // Push the assigned value back for expression result
                        // (Regenerate the index access to get the value)
                        self.generate_expr(func, array_expr, type_table, ctx, builder);
                        match &array_kind {
                            ArrayKind::Array { struct_type_idx } => {
                                func.instruction(&Instruction::StructGet {
                                    struct_type_index: *struct_type_idx,
                                    field_index: 0,
                                });
                            }
                            ArrayKind::String => {
                                if let Some(struct_info) =
                                    self.lookup_struct_type("String", &string_module_path())
                                {
                                    func.instruction(&Instruction::StructGet {
                                        struct_type_index: struct_info.type_idx,
                                        field_index: 0,
                                    });
                                }
                            }
                        }
                        self.generate_expr(func, index_expr, type_table, ctx, builder);
                        func.instruction(&Instruction::ArrayGet(raw_array_type_idx));
                    }
                    TirExprKind::Unary {
                        op: TirUnaryOp::Deref,
                        expr: ref_expr,
                    } => {
                        // Assignment through dereference: *x = value
                        // For primitive refs: update the box struct
                        // For struct/tuple refs: this assigns the whole value (not field)
                        let ref_type = type_table.get(ref_expr.type_id);
                        if let ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) = ref_type {
                            if let ResolvedType::Primitive(prim) = type_table.get(*inner) {
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
                if let Some(builtin) = call_func.builtin_name() {
                    self.generate_builtin_call(
                        &builtin, args, expr, func, type_table, ctx, builder,
                    );
                } else if module_path.is_empty()
                    && self.generate_variant_constructor(
                        &func_name, args, func, type_table, ctx, builder,
                    )
                {
                    // Variant constructor was handled
                } else if (module_path == ["Stdout"] || module_path == ["Stderr"])
                    && func_name == "write_via_stream"
                {
                    // Direct effect operation call: Stdout::write_via_stream(rx) or Stderr::write_via_stream(rx)
                    // These need special handling because WASI P3 async operations require:
                    // 1. An extra outptr argument
                    // 2. Storing the result (subtask handle) for later waiting

                    // Generate the rx argument
                    for arg in args {
                        self.generate_expr(func, arg, type_table, ctx, builder);
                    }

                    // Add the outptr argument (WASI P3 async operations need this)
                    func.instruction(&Instruction::I32Const(2048));

                    // Resolve the WASI function
                    let effect_name = &module_path[0];
                    let local_name = build_local_alias_name("cli", effect_name, &func_name);
                    let func_idx = builder.func_idx(&local_name);
                    func.instruction(&Instruction::Call(func_idx));

                    // Store subtask handle in the pre-allocated local for later waiting
                    let subtask_local = ctx.get_local("__subtask").expect(
                        "__subtask should be pre-allocated for functions with Stdout/Stderr effects",
                    );
                    func.instruction(&Instruction::LocalSet(subtask_local));
                } else if module_path == ["Environment"]
                    && matches!(func_name.as_str(), "get_arguments" | "get_environment")
                {
                    // Environment operations that return list<string> or list<tuple<string, string>>
                    // CM ABI: function takes outptr, writes (base_ptr, count) to it
                    // We need to convert to GC array

                    // Allocate outptr for CM result (8 bytes: ptr + count)
                    func.instruction(&Instruction::I32Const(0)); // old_ptr
                    func.instruction(&Instruction::I32Const(0)); // old_size
                    func.instruction(&Instruction::I32Const(4)); // align
                    func.instruction(&Instruction::I32Const(8)); // new_size
                    let realloc_idx = builder.func_idx("realloc");
                    func.instruction(&Instruction::Call(realloc_idx));

                    // Store outptr in a local for later use
                    let outptr_local = ctx.get_local("__cm_outptr").expect(
                        "__cm_outptr should be pre-allocated for functions with Environment calls",
                    );
                    func.instruction(&Instruction::LocalTee(outptr_local));

                    // Call the WASI function with outptr
                    let local_name = build_local_alias_name("cli", "Environment", &func_name);
                    let func_idx = builder.func_idx(&local_name);
                    func.instruction(&Instruction::Call(func_idx));

                    // Load outptr and call conversion function
                    func.instruction(&Instruction::LocalGet(outptr_local));
                    let conv_idx = builder.func_idx("core/internal/cm_list_string_to_array");
                    func.instruction(&Instruction::Call(conv_idx));
                } else if module_path == ["Environment"] && func_name.as_str() == "get_initial_cwd"
                {
                    // get_initial_cwd returns Option<String>
                    // CM ABI: function takes outptr, writes option<string> to it
                    // Layout: discriminant (1 byte at offset 0, padded) + str_ptr (4 bytes) + str_len (4 bytes)
                    // Total: 12 bytes

                    // Allocate outptr for CM result (12 bytes)
                    func.instruction(&Instruction::I32Const(0)); // old_ptr
                    func.instruction(&Instruction::I32Const(0)); // old_size
                    func.instruction(&Instruction::I32Const(4)); // align
                    func.instruction(&Instruction::I32Const(12)); // new_size
                    let realloc_idx = builder.func_idx("realloc");
                    func.instruction(&Instruction::Call(realloc_idx));

                    // Store outptr in a local for later use
                    let outptr_local = ctx.get_local("__cm_outptr").expect(
                        "__cm_outptr should be pre-allocated for functions with Environment calls",
                    );
                    func.instruction(&Instruction::LocalTee(outptr_local));

                    // Call the WASI function with outptr
                    let local_name = build_local_alias_name("cli", "Environment", &func_name);
                    let func_idx = builder.func_idx(&local_name);
                    func.instruction(&Instruction::Call(func_idx));

                    // Load outptr and call conversion function
                    func.instruction(&Instruction::LocalGet(outptr_local));
                    let conv_idx = builder.func_idx("core/internal/cm_option_string_to_option");
                    func.instruction(&Instruction::Call(conv_idx));
                } else {
                    // Generate arguments first
                    for arg in args {
                        self.generate_expr(func, arg, type_table, ctx, builder);
                    }

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
            TirExprKind::EffectCall {
                effect_name,
                op_name,
                args,
            } => {
                // Special handling for async WASI operations
                if (effect_name == "Stdout" || effect_name == "Stderr")
                    && op_name == "write_via_stream"
                {
                    // Generate the rx argument
                    for arg in args {
                        self.generate_expr(func, arg, type_table, ctx, builder);
                    }

                    // Add the outptr argument (WASI P3 async operations need this)
                    func.instruction(&Instruction::I32Const(2048));

                    // Resolve the WASI function
                    let local_name = build_local_alias_name("cli", effect_name, op_name);
                    let func_idx = builder.func_idx(&local_name);
                    func.instruction(&Instruction::Call(func_idx));

                    // Store subtask handle in the pre-allocated local for later waiting
                    let subtask_local = ctx.get_local("__subtask").expect(
                        "__subtask should be pre-allocated for functions with Stdout/Stderr effects",
                    );
                    func.instruction(&Instruction::LocalSet(subtask_local));
                } else if effect_name == "Environment"
                    && matches!(op_name.as_str(), "get_arguments" | "get_environment")
                {
                    // Environment operations that return list<string> or list<tuple<string, string>>
                    // CM ABI: function takes outptr, writes (base_ptr, count) to it
                    // We need to convert to GC array

                    // Allocate outptr for CM result (8 bytes: ptr + count)
                    func.instruction(&Instruction::I32Const(0)); // old_ptr
                    func.instruction(&Instruction::I32Const(0)); // old_size
                    func.instruction(&Instruction::I32Const(4)); // align
                    func.instruction(&Instruction::I32Const(8)); // new_size
                    let realloc_idx = builder.func_idx("realloc");
                    func.instruction(&Instruction::Call(realloc_idx));

                    // Store outptr in a local for later use
                    let outptr_local = ctx.get_local("__cm_outptr").expect(
                        "__cm_outptr should be pre-allocated for functions with Environment calls",
                    );
                    func.instruction(&Instruction::LocalTee(outptr_local));

                    // Call the WASI function with outptr
                    let local_name = build_local_alias_name("cli", effect_name, op_name);
                    let func_idx = builder.func_idx(&local_name);
                    func.instruction(&Instruction::Call(func_idx));

                    // Load outptr and call conversion function
                    func.instruction(&Instruction::LocalGet(outptr_local));
                    let conv_idx = builder.func_idx("core/internal/cm_list_string_to_array");
                    func.instruction(&Instruction::Call(conv_idx));
                } else if effect_name == "Environment" && op_name == "get_initial_cwd" {
                    // get_initial_cwd returns Option<String>
                    // CM ABI: function takes outptr, writes option<string> to it
                    // Layout: discriminant (1 byte at offset 0, padded) + str_ptr (4 bytes) + str_len (4 bytes)
                    // Total: 12 bytes

                    // Allocate outptr for CM result (12 bytes)
                    func.instruction(&Instruction::I32Const(0)); // old_ptr
                    func.instruction(&Instruction::I32Const(0)); // old_size
                    func.instruction(&Instruction::I32Const(4)); // align
                    func.instruction(&Instruction::I32Const(12)); // new_size
                    let realloc_idx = builder.func_idx("realloc");
                    func.instruction(&Instruction::Call(realloc_idx));

                    // Store outptr in a local for later use
                    let outptr_local = ctx.get_local("__cm_outptr").expect(
                        "__cm_outptr should be pre-allocated for functions with Environment calls",
                    );
                    func.instruction(&Instruction::LocalTee(outptr_local));

                    // Call the WASI function with outptr
                    let local_name = build_local_alias_name("cli", effect_name, op_name);
                    let func_idx = builder.func_idx(&local_name);
                    func.instruction(&Instruction::Call(func_idx));

                    // Load outptr and call conversion function
                    func.instruction(&Instruction::LocalGet(outptr_local));
                    let conv_idx = builder.func_idx("core/internal/cm_option_string_to_option");
                    func.instruction(&Instruction::Call(conv_idx));
                } else {
                    // Regular effect call
                    for arg in args {
                        self.generate_expr(func, arg, type_table, ctx, builder);
                    }
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
                // Extract method name and trait name from func reference
                // Format can be "StructName::method" or "StructName^TraitName::method"
                let (method_name, trait_name) = {
                    let name = method_func.name();
                    if let Some(pos) = name.rfind("::") {
                        let method = name[pos + 2..].to_string();
                        let prefix = &name[..pos];
                        // Check for trait impl: "StructName^TraitName"
                        let trait_n = prefix
                            .find('^')
                            .map(|caret_pos| prefix[caret_pos + 1..].to_string());
                        (method, trait_n)
                    } else {
                        (name, None)
                    }
                };
                // Get the base type for method lookup (strip Ref/MutRef)
                let base_receiver_type = {
                    let mut t = type_table.get(receiver.type_id).clone();
                    while let ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) = t {
                        t = type_table.get(inner).clone();
                    }
                    t
                };

                match base_receiver_type {
                    // Struct method call
                    ResolvedType::Struct {
                        ref name,
                        ref module_path,
                    } if name == "String" && module_path == &string_module_path() => {
                        // String struct - handle specially like legacy ResolvedType::String
                        match method_name.as_str() {
                            "len" => {
                                // Generate the receiver (the string)
                                self.generate_expr(func, receiver, type_table, ctx, builder);
                                // Call String::len method
                                let len_func_idx = builder.func_idx("core/prelude/String::len");
                                func.instruction(&Instruction::Call(len_func_idx));
                            }
                            "get" => {
                                // string.get(index) -> call String::get method
                                self.generate_expr(func, receiver, type_table, ctx, builder);
                                if let Some(index_arg) = args.first() {
                                    self.generate_expr(func, index_arg, type_table, ctx, builder);
                                }
                                let get_func_idx = builder.func_idx("core/prelude/String::get");
                                func.instruction(&Instruction::Call(get_func_idx));
                            }
                            "set" => {
                                // string.set(index, value) -> call String::set method
                                self.generate_expr(func, receiver, type_table, ctx, builder);
                                if let Some(index_arg) = args.first() {
                                    self.generate_expr(func, index_arg, type_table, ctx, builder);
                                }
                                if let Some(value_arg) = args.get(1) {
                                    self.generate_expr(func, value_arg, type_table, ctx, builder);
                                }
                                let set_func_idx = builder.func_idx("core/prelude/String::set");
                                func.instruction(&Instruction::Call(set_func_idx));
                            }
                            _ => {
                                panic!("unknown method {method_name} on String type");
                            }
                        }
                    }

                    ResolvedType::Struct { name, module_path } => {
                        // Build the fully mangled method name: path/Struct^Trait::method or path/Struct::method
                        let mangled_name = MethodName::new(
                            module_path.join("/"),
                            name.clone(),
                            trait_name.clone(),
                            method_name.clone(),
                        )
                        .to_string();

                        // Look up the method function index
                        // For monomorphized generics (e.g., Box<i32>), also try base struct name (Box)
                        let struct_lookup_name = StructName::new(module_path.clone(), name.clone());
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

                        if let Some(idx) = func_idx {
                            // Generate code for the receiver (self parameter)
                            self.generate_expr(func, receiver, type_table, ctx, builder);

                            // Generate code for other arguments
                            for arg in args {
                                self.generate_expr(func, arg, type_table, ctx, builder);
                            }

                            // Call the method
                            func.instruction(&Instruction::Call(idx));
                        } else {
                            // Method not found - also try the simple alias name
                            // Monomorphized methods are registered with an alias using just
                            // the struct name and method (e.g., "Pair<i32,i64>::get_first")
                            let simple_name = format!("{name}::{method_name}");
                            if let Some(idx) = builder.try_func_idx(&simple_name) {
                                // Generate code for the receiver (self parameter)
                                self.generate_expr(func, receiver, type_table, ctx, builder);
                                // Generate code for other arguments
                                for arg in args {
                                    self.generate_expr(func, arg, type_table, ctx, builder);
                                }
                                // Call the method
                                func.instruction(&Instruction::Call(idx));
                            } else {
                                panic!(
                                    "unknown method: {mangled_name} (also tried alias: {simple_name})"
                                );
                            }
                        }
                    }

                    // Primitive method calls (e.g., i32.to_string())
                    ResolvedType::Primitive(prim) => {
                        if method_name == "to_string" {
                            // Generate the receiver value first
                            self.generate_expr(func, receiver, type_table, ctx, builder);

                            // Call the appropriate builtin to_string function
                            let func_name = match prim {
                                PrimitiveType::I32 | PrimitiveType::I8 | PrimitiveType::I16 => {
                                    "core/internal/i32_to_string"
                                }
                                PrimitiveType::U32 | PrimitiveType::U8 | PrimitiveType::U16 => {
                                    "core/internal/u32_to_string"
                                }
                                PrimitiveType::I64 => "core/internal/i64_to_string",
                                PrimitiveType::U64 => "core/internal/u64_to_string",
                                PrimitiveType::F32 => "core/internal/f32_to_string",
                                PrimitiveType::F64 => "core/internal/f64_to_string",
                                PrimitiveType::Bool => "core/internal/bool_to_string",
                                PrimitiveType::Char => "core/internal/char_to_string",
                                _ => {
                                    panic!("to_string not supported for primitive type: {prim:?}")
                                }
                            };
                            if let Some(func_idx) = builder.try_func_idx(func_name) {
                                func.instruction(&Instruction::Call(func_idx));
                            } else {
                                panic!("missing builtin function: {func_name}");
                            }
                        } else {
                            panic!(
                                "unknown method {method_name} on primitive type {prim:?}"
                            );
                        }
                    }

                    // Array method calls - call the monomorphized methods
                    // Monomorphized methods are registered with aliases using their simple name
                    // (e.g., Array<String>::len) regardless of which module they're in
                    ResolvedType::GenericInstance {
                        name, type_args, ..
                    } if name == "Array" && type_args.len() == 1 => {
                        let element_type = type_args[0];
                        // Build mangled method name: Array<String>::len
                        let elem_name = self.mangle_type_for_struct_name(element_type, type_table);
                        let func_name = format!("Array<{elem_name}>::{method_name}");

                        // Generate receiver
                        self.generate_expr(func, receiver, type_table, ctx, builder);
                        // Generate arguments
                        for arg in args {
                            self.generate_expr(func, arg, type_table, ctx, builder);
                        }
                        // Call the monomorphized method
                        let func_idx = builder.func_idx(&func_name);
                        func.instruction(&Instruction::Call(func_idx));
                    }

                    // String method calls - String is now a struct, call the struct methods
                    ResolvedType::String => {
                        match method_name.as_str() {
                            "len" => {
                                // Generate the receiver (the string)
                                self.generate_expr(func, receiver, type_table, ctx, builder);
                                // Call String::len method
                                let len_func_idx = builder.func_idx("core/prelude/String::len");
                                func.instruction(&Instruction::Call(len_func_idx));
                            }
                            "get" => {
                                // string.get(index) -> call String::get method
                                // Generate receiver (string)
                                self.generate_expr(func, receiver, type_table, ctx, builder);
                                // Generate index argument
                                if let Some(index_arg) = args.first() {
                                    self.generate_expr(func, index_arg, type_table, ctx, builder);
                                }
                                let get_func_idx = builder.func_idx("core/prelude/String::get");
                                func.instruction(&Instruction::Call(get_func_idx));
                            }
                            "set" => {
                                // string.set(index, value) -> call String::set method
                                // Generate receiver (string)
                                self.generate_expr(func, receiver, type_table, ctx, builder);
                                // Generate index argument
                                if let Some(index_arg) = args.first() {
                                    self.generate_expr(func, index_arg, type_table, ctx, builder);
                                }
                                // Generate value argument
                                if let Some(value_arg) = args.get(1) {
                                    self.generate_expr(func, value_arg, type_table, ctx, builder);
                                }
                                let set_func_idx = builder.func_idx("core/prelude/String::set");
                                func.instruction(&Instruction::Call(set_func_idx));
                            }
                            _ => {
                                panic!("unknown method {method_name} on String type");
                            }
                        }
                    }

                    // User-defined generic struct method calls (e.g., Box<i32>.get())
                    ResolvedType::GenericInstance {
                        name,
                        type_args,
                        module_path,
                    } => {
                        // Build monomorphized struct and method name: Box<i32>::get
                        let type_arg_names: Vec<String> = type_args
                            .iter()
                            .map(|t| self.mangle_type_for_struct_name(*t, type_table))
                            .collect();
                        let mangled_struct_name = format!("{}<{}>", name, type_arg_names.join(","));
                        let mangled_method_name =
                            format!("{mangled_struct_name}::{method_name}");

                        // Build full method name with module path
                        let full_method_name = MethodName::new(
                            module_path.join("/"),
                            mangled_struct_name.clone(),
                            None,
                            method_name.clone(),
                        )
                        .to_string();

                        // Try full method name first, then simple name
                        let func_idx = builder
                            .try_func_idx(&full_method_name)
                            .or_else(|| builder.try_func_idx(&mangled_method_name));

                        if let Some(idx) = func_idx {
                            // Generate receiver
                            self.generate_expr(func, receiver, type_table, ctx, builder);
                            // Generate arguments
                            for arg in args {
                                self.generate_expr(func, arg, type_table, ctx, builder);
                            }
                            // Call the method
                            func.instruction(&Instruction::Call(idx));
                        } else {
                            panic!(
                                "unknown method {method_name} on generic struct {name}: tried {full_method_name} and {mangled_method_name}"
                            );
                        }
                    }

                    other => {
                        panic!(
                            "method call receiver is not a struct or primitive type: {:?}, method: {}, receiver.type_id: {}",
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
                let module_path = static_func.module_path();

                // Generate arguments first
                for arg in args {
                    self.generate_expr(func, arg, type_table, ctx, builder);
                }

                // Check if this is a monomorphized function using metadata
                let base_struct_name = static_func.base_struct_name();

                // func_name is already mangled as "StructName::method" or "Struct<Type>::method"
                // We need to look it up using the same name format used during function definition
                // Methods are registered with MethodName format: {module_path}/{struct_name}::{method_name}
                let func_idx = if let Some(sep_pos) = func_name.find("::") {
                    let struct_name = &func_name[..sep_pos];
                    let method_name = &func_name[sep_pos + 2..];

                    // Build the mangled name in the same format as during function definition
                    let mangled_name = MethodName::new(
                        module_path.join("/"),
                        struct_name.to_string(),
                        None,
                        method_name.to_string(),
                    )
                    .to_string();

                    // Check struct metadata for fallback
                    let struct_lookup_name =
                        StructName::new(module_path.clone(), struct_name.to_string());
                    let struct_info = self.struct_types.get(&struct_lookup_name);

                    builder
                        .try_func_idx(&mangled_name)
                        .or_else(|| {
                            // Also try without module path (for current module lookups)
                            builder.try_func_idx(&func_name)
                        })
                        .or_else(|| {
                            // For monomorphized generic types like Array<i32>, also try the generic version Array
                            // This handles static methods on generic types that aren't monomorphized
                            // Use metadata: either from function or struct, not string parsing
                            let generic_name = base_struct_name
                                .as_ref()
                                .or_else(|| struct_info.and_then(|s| s.base_name.as_ref()));

                            if let Some(generic_struct_name) = generic_name {
                                let generic_mangled_name = MethodName::new(
                                    module_path.join("/"),
                                    generic_struct_name.clone(),
                                    None,
                                    method_name.to_string(),
                                )
                                .to_string();
                                builder.try_func_idx(&generic_mangled_name)
                            } else {
                                None
                            }
                        })
                } else {
                    // No :: separator, try as a regular function call
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
                            module_path: type_module_path,
                            type_args,
                        } => {
                            // Build the mangled struct name (e.g., Box<i32>)
                            let type_arg_names: Vec<String> = type_args
                                .iter()
                                .map(|t| self.mangle_type_for_struct_name(*t, type_table))
                                .collect();
                            let mangled = format!("{}<{}>", name, type_arg_names.join(","));
                            Some((mangled, type_module_path.clone()))
                        }
                        ResolvedType::Struct {
                            name,
                            module_path: type_module_path,
                        } => {
                            // Check if this struct is monomorphized using metadata
                            let struct_lookup =
                                StructName::new(type_module_path.clone(), name.clone());
                            if self
                                .struct_types
                                .get(&struct_lookup)
                                .map(|s| s.is_monomorphized)
                                .unwrap_or(false)
                            {
                                Some((name.clone(), type_module_path.clone()))
                            } else {
                                None
                            }
                        }
                        _ => None,
                    };

                    if let Some((struct_name_to_lookup, struct_module_path)) = return_type_info {
                        // Look up the struct type
                        let struct_lookup_name =
                            StructName::new(struct_module_path, struct_name_to_lookup);
                        if let Some(struct_info) = self.struct_types.get(&struct_lookup_name) {
                            // Check if args count matches field count (constructor pattern)
                            if args.len() == struct_info.field_count {
                                // Arguments are already on the stack, just create the struct
                                func.instruction(&Instruction::StructNew(struct_info.type_idx));
                                return;
                            }
                        }
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
                let base_type = type_table.get(base_type_id);
                // Track element type info for post-array-get processing
                let (raw_array_type_idx, element_is_ref, closure_cast_type_idx) =
                    if let Some(element_type) = type_table.as_array(base_type_id) {
                        let element_resolved = type_table.get(element_type);
                        let is_ref = matches!(
                            element_resolved,
                            ResolvedType::String
                                | ResolvedType::GenericInstance { .. }
                                | ResolvedType::Struct { .. }
                                | ResolvedType::Function { .. }
                        );
                        // For function types, we need to cast structref to canonical closure type
                        let closure_type_idx = if let ResolvedType::Function {
                            params,
                            return_type,
                            ..
                        } = element_resolved
                        {
                            let canonical = self.canonical_closure_types.borrow();
                            canonical
                                .get(&(params.clone(), *return_type))
                                .map(|(_, _, struct_idx)| *struct_idx)
                        } else {
                            None
                        };
                        let array_struct_type_idx = self
                            .array_struct_types
                            .get(&element_type)
                            .expect("Array struct type should be registered");
                        // Access the repr field (field 0) to get the raw array
                        func.instruction(&Instruction::StructGet {
                            struct_type_index: *array_struct_type_idx,
                            field_index: 0, // repr is field 0
                        });
                        (
                            self.array_types
                                .get(&element_type)
                                .copied()
                                .unwrap_or(self.string_array_type_idx),
                            is_ref,
                            closure_type_idx,
                        )
                    } else if let ResolvedType::String = base_type {
                        // String is now a struct with repr field (field 0) containing the array
                        // First access the repr field, then do array.get
                        if let Some(struct_info) =
                            self.lookup_struct_type("String", &string_module_path())
                        {
                            func.instruction(&Instruction::StructGet {
                                struct_type_index: struct_info.type_idx,
                                field_index: 0, // repr is field 0
                            });
                        }
                        (self.string_array_type_idx, false, None)
                    } else {
                        (self.string_array_type_idx, false, None)
                    };

                // Now generate index and do array access
                self.generate_expr(func, index, type_table, ctx, builder);
                func.instruction(&Instruction::ArrayGet(raw_array_type_idx));
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
                self.generate_block(func, block, type_table, ctx, builder);
            }

            // === If Expression ===
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.generate_expr(func, condition, type_table, ctx, builder);
                let result_type = self.type_id_to_valtype(type_table, expr.type_id);
                func.instruction(&Instruction::If(wasm_encoder::BlockType::Result(
                    result_type,
                )));
                self.generate_block(func, then_branch, type_table, ctx, builder);
                if let Some(else_block) = else_branch {
                    func.instruction(&Instruction::Else);
                    self.generate_block(func, else_block, type_table, ctx, builder);
                }
                func.instruction(&Instruction::End);
            }

            // === Match Expression ===
            TirExprKind::Match {
                expr: scrutinee,
                arms: _,
            } => {
                // TODO: Implement proper pattern matching
                self.generate_expr(func, scrutinee, type_table, ctx, builder);
                panic!("match expressions not yet implemented in TIR codegen");
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
                    ResolvedType::Struct { name, module_path } => {
                        self.lookup_struct_type(name, module_path)
                    }
                    ResolvedType::GenericInstance {
                        name, type_args, ..
                    } if name == "Array" && type_args.len() == 1 => {
                        // Array<T> struct literal - use the monomorphized Array struct type
                        let elem_type = type_args[0];
                        if let Some(&array_struct_type_idx) =
                            self.array_struct_types.get(&elem_type)
                        {
                            // Create inline StructTypeInfo for the Array struct
                            // We store it on the stack and return a reference
                            func.instruction(&Instruction::StructNew(array_struct_type_idx));
                            return;
                        } else {
                            // Fall back to simple name lookup
                            self.lookup_struct_type(struct_name, &[])
                        }
                    }
                    ResolvedType::GenericInstance {
                        name,
                        type_args,
                        module_path,
                    } => {
                        // Generic struct literal - look up the monomorphized struct name
                        let type_arg_names: Vec<String> = type_args
                            .iter()
                            .map(|t| self.mangle_type_for_struct_name(*t, type_table))
                            .collect();
                        let mangled_name = format!("{}<{}>", name, type_arg_names.join(","));
                        self.lookup_struct_type(&mangled_name, module_path)
                    }
                    _ => {
                        // Fall back to simple name lookup using struct_name
                        self.lookup_struct_type(struct_name, &[])
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
                    let raw_array_type_idx = self
                        .array_types
                        .get(&element_type_id)
                        .copied()
                        .unwrap_or(self.string_array_type_idx);

                    let array_struct_type_idx = self
                        .array_struct_types
                        .get(&element_type_id)
                        .copied()
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

            // === Capture ===
            TirExprKind::Capture { index, name } => {
                // Capture access: get the captured value from the environment struct.
                // The environment is always the first parameter (local 0) in closure functions.
                // Since the function type uses generic (ref struct), we need to cast to the
                // specific env type before accessing fields.
                let env_type_idx = ctx.closure_env_type_idx.unwrap_or_else(|| {
                    panic!(
                        "capture access for '{name}' (index {index}) outside of closure context"
                    )
                });

                // Get env from local 0 (first parameter in closure functions)
                func.instruction(&Instruction::LocalGet(0));
                // Cast generic (ref struct) to specific env type
                func.instruction(&Instruction::RefCastNonNull(HeapType::Concrete(
                    env_type_idx,
                )));
                // Get the captured value from the environment struct
                func.instruction(&Instruction::StructGet {
                    struct_type_index: env_type_idx,
                    field_index: *index,
                });
            }

            // === Closure ===
            TirExprKind::Closure {
                params: _,
                body: _,
                captures,
            } => {
                // Get the closure ID and look up its registered info
                let closure_id = {
                    let mut counter = self.closure_codegen_counter.borrow_mut();
                    let id = *counter;
                    *counter += 1;
                    id
                };

                let pending = self.pending_closures.borrow();
                let closure_info = pending.get(closure_id as usize).unwrap_or_else(|| {
                    panic!(
                        "closure {} not found in pending_closures (have {})",
                        closure_id,
                        pending.len()
                    )
                });

                let env_type_idx = closure_info.env_type_idx;
                let closure_struct_type_idx = closure_info.closure_struct_type_idx;
                let func_idx = closure_info.func_idx;

                // Push captured values onto the stack in order
                for capture in captures {
                    // Get the value from the outer function's local
                    func.instruction(&Instruction::LocalGet(capture.outer_index));
                }

                // Create the environment struct
                func.instruction(&Instruction::StructNew(env_type_idx));

                // Create the closure struct (env + funcref)
                // Stack now has: env_ref
                // We need to create a struct with (env, funcref)
                func.instruction(&Instruction::RefFunc(func_idx));
                func.instruction(&Instruction::StructNew(closure_struct_type_idx));
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
                        self.canonical_closure_types.borrow().get(&key).cloned()
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
                // We use a counter to ensure nested calls don't share the same local.
                let call_id = ctx.indirect_call_counter;
                ctx.indirect_call_counter += 1;
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
                for arg in args {
                    self.generate_expr(func, arg, type_table, ctx, builder);
                }

                // Get the closure again and extract funcref (field 1)
                func.instruction(&Instruction::LocalGet(closure_local));
                func.instruction(&Instruction::StructGet {
                    struct_type_index: closure_struct_type_idx,
                    field_index: 1,
                });

                // Call via call_ref with the function type
                func.instruction(&Instruction::CallRef(fn_type_idx));
            }
        }
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
                let (raw_array_type_idx, array_kind) =
                    if let Some(element_type) = type_table.as_array(base_type_id) {
                        let raw_type_idx = self
                            .array_types
                            .get(&element_type)
                            .copied()
                            .unwrap_or(self.string_array_type_idx);
                        let struct_type_idx = *self
                            .array_struct_types
                            .get(&element_type)
                            .expect("Array struct type should be registered");
                        (raw_type_idx, ArrayKind::Array { struct_type_idx })
                    } else if let ResolvedType::String = base_type {
                        (self.string_array_type_idx, ArrayKind::String)
                    } else {
                        panic!("index assignment on non-array type: {base_type:?}");
                    };

                self.generate_expr(func, array_expr, type_table, ctx, builder);
                match &array_kind {
                    ArrayKind::Array { struct_type_idx } => {
                        func.instruction(&Instruction::StructGet {
                            struct_type_index: *struct_type_idx,
                            field_index: 0,
                        });
                    }
                    ArrayKind::String => {
                        if let Some(struct_info) =
                            self.lookup_struct_type("String", &string_module_path())
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

    /// Try to generate an inverted condition for `br_if` optimization.
    /// Returns true if the condition was inverted and generated, false otherwise.
    /// This eliminates the pattern: condition + i32.eqz + `br_if`
    /// by directly generating: `inverted_condition` + `br_if`
    fn try_generate_inverted_condition(
        &self,
        func: &mut Function,
        condition: &TirExpr,
        type_table: &TypeTable,
        ctx: &mut FunctionContext,
        builder: &CoreModuleBuilder,
    ) -> bool {
        // Check if condition is a simple binary comparison that can be inverted
        if let TirExprKind::Binary { left, op, right } = &condition.kind {
            let inverted_op = match op {
                TirBinaryOp::Lt => Some(TirBinaryOp::GtEq),
                TirBinaryOp::LtEq => Some(TirBinaryOp::Gt),
                TirBinaryOp::Gt => Some(TirBinaryOp::LtEq),
                TirBinaryOp::GtEq => Some(TirBinaryOp::Lt),
                TirBinaryOp::Eq => Some(TirBinaryOp::NotEq),
                TirBinaryOp::NotEq => Some(TirBinaryOp::Eq),
                _ => None,
            };

            if let Some(inv_op) = inverted_op {
                // Generate left operand
                self.generate_expr(func, left, type_table, ctx, builder);
                // Generate right operand
                self.generate_expr(func, right, type_table, ctx, builder);
                // Determine the effective type for the comparison
                // (same logic as in generate_binary_op for comparison signedness)
                let is_left_unsigned = matches!(
                    type_table.get(left.type_id),
                    ResolvedType::Primitive(
                        PrimitiveType::U8
                            | PrimitiveType::U16
                            | PrimitiveType::U32
                            | PrimitiveType::U64
                    )
                );
                let is_right_unsigned = matches!(
                    type_table.get(right.type_id),
                    ResolvedType::Primitive(
                        PrimitiveType::U8
                            | PrimitiveType::U16
                            | PrimitiveType::U32
                            | PrimitiveType::U64
                    )
                );
                let effective_type = if is_left_unsigned || is_right_unsigned {
                    let is_i64 = matches!(
                        type_table.get(left.type_id),
                        ResolvedType::Primitive(PrimitiveType::I64 | PrimitiveType::U64)
                    );
                    if is_i64 { TypeTable::U64 } else { left.type_id }
                } else {
                    left.type_id
                };
                // Generate the inverted comparison
                self.generate_binary_op(func, inv_op, effective_type, type_table);
                return true;
            }
        }

        // Check if condition is a negation (!expr) - we can just use the inner expr
        if let TirExprKind::Unary {
            op: TirUnaryOp::Not,
            expr: inner,
        } = &condition.kind
        {
            self.generate_expr(func, inner, type_table, ctx, builder);
            return true;
        }

        false
    }

    /// Generate code for a TIR binary operation
    fn generate_binary_op(
        &self,
        func: &mut Function,
        op: TirBinaryOp,
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
        let is_unsigned = matches!(
            type_table.get(operand_type),
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
                // For primitives, box the value in a single-field struct
                // For GC types (structs, arrays, tuples), references are transparent
                if let ResolvedType::Primitive(prim) = type_table.get(operand_type) {
                    let val_type = primitive_to_valtype(prim);
                    if let Some(box_type_idx) = self.get_box_type_idx(val_type) {
                        func.instruction(&Instruction::StructNew(box_type_idx));
                    }
                    // else: no box type for this primitive, treat as transparent
                }
                // For non-primitives (structs, arrays, tuples), no operation needed
            }
            TirUnaryOp::Deref => {
                // For references to primitives, unbox by extracting from the box struct
                // For references to GC types, references are transparent
                if let ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) =
                    type_table.get(operand_type)
                    && let ResolvedType::Primitive(prim) = type_table.get(*inner)
                {
                    let val_type = primitive_to_valtype(prim);
                    if let Some(box_type_idx) = self.get_box_type_idx(val_type) {
                        func.instruction(&Instruction::StructGet {
                            struct_type_index: box_type_idx,
                            field_index: 0,
                        });
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
            // i32 -> i64
            (
                ResolvedType::Primitive(PrimitiveType::I32),
                ResolvedType::Primitive(PrimitiveType::I64),
            ) => {
                func.instruction(&Instruction::I64ExtendI32S);
            }
            // i64 -> i32 (truncate)
            (
                ResolvedType::Primitive(PrimitiveType::I64),
                ResolvedType::Primitive(PrimitiveType::I32),
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
                // Skip for Move expressions (optimizer marks fresh values with Move)
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

            TirStmtKind::Expr(expr) => {
                // Use optimized statement generation to avoid drop-tee pattern
                self.generate_expr_as_stmt(func, expr, type_table, ctx, builder);
            }

            TirStmtKind::Return { value } => {
                if let Some(expr) = value {
                    self.generate_expr(func, expr, type_table, ctx, builder);
                }
                func.instruction(&Instruction::Return);
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
                if let Some((extra, _)) = ctx.loop_info.last_mut() {
                    *extra += 1;
                }
                self.generate_block(func, then_block, type_table, ctx, builder);
                if let Some(else_blk) = else_block {
                    func.instruction(&Instruction::Else);
                    // Else branch is at the same depth as then branch
                    self.generate_block(func, else_blk, type_table, ctx, builder);
                }
                if let Some((extra, _)) = ctx.loop_info.last_mut() {
                    *extra -= 1;
                }
                func.instruction(&Instruction::End);
            }

            TirStmtKind::While { condition, body } => {
                // Push new loop context: (extra_depth=0, break_offset=1)
                ctx.loop_info.push((0, 1));

                func.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
                func.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));

                // Check condition, break if false
                // Try to generate inverted condition directly to avoid i32.eqz
                let (extra, break_offset) = *ctx.loop_info.last().unwrap();
                if !self.try_generate_inverted_condition(func, condition, type_table, ctx, builder)
                {
                    // Fallback: generate condition and negate
                    self.generate_expr(func, condition, type_table, ctx, builder);
                    func.instruction(&Instruction::I32Eqz);
                }
                func.instruction(&Instruction::BrIf(break_offset + extra));

                // Execute body
                self.generate_block(func, body, type_table, ctx, builder);

                // Continue loop
                let (extra, _) = *ctx.loop_info.last().unwrap();
                func.instruction(&Instruction::Br(extra));

                func.instruction(&Instruction::End); // End loop
                func.instruction(&Instruction::End); // End block

                ctx.loop_info.pop();
            }

            TirStmtKind::Loop { body } => {
                // Push new loop context: (extra_depth=0, break_offset=1)
                ctx.loop_info.push((0, 1));

                func.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
                func.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));

                self.generate_block(func, body, type_table, ctx, builder);

                // Continue loop
                let (extra, _) = *ctx.loop_info.last().unwrap();
                func.instruction(&Instruction::Br(extra));

                func.instruction(&Instruction::End); // End loop
                func.instruction(&Instruction::End); // End block

                ctx.loop_info.pop();
            }

            TirStmtKind::For {
                condition,
                body,
                update,
            } => {
                // For loop structure:
                // block $exit        ; break target
                //   loop $loop       ; for loop header
                //     ;; condition check (if present)
                //     block $body    ; continue target
                //       ;; body
                //     end
                //     ;; update (if present)
                //     br $loop
                //   end
                // end
                //
                // From inside body:
                // - continue: br 0 (to end of $body, then update executes, then br $loop)
                // - break: br 2 (to $exit)

                // Push new loop context: (extra_depth=0, break_offset=2)
                // break_offset=2 because break needs to skip body block + loop
                ctx.loop_info.push((0, 2));

                func.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
                func.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));

                // Check condition if present
                if let Some(cond) = condition {
                    // Try to generate inverted condition directly to avoid i32.eqz
                    if !self.try_generate_inverted_condition(func, cond, type_table, ctx, builder) {
                        // Fallback: generate condition and negate
                        self.generate_expr(func, cond, type_table, ctx, builder);
                        func.instruction(&Instruction::I32Eqz);
                    }
                    // At this point we're not inside the body block yet, so br 1 exits to $exit
                    func.instruction(&Instruction::BrIf(1));
                }

                // Body block (continue target)
                func.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));

                self.generate_block(func, body, type_table, ctx, builder);

                func.instruction(&Instruction::End); // End body block

                // Update expression if present
                if let Some(upd) = update {
                    // Use optimized statement generation to avoid drop-tee pattern
                    self.generate_expr_as_stmt(func, upd, type_table, ctx, builder);
                }

                // Continue to loop header (br 0 from here)
                func.instruction(&Instruction::Br(0));

                func.instruction(&Instruction::End); // End loop
                func.instruction(&Instruction::End); // End exit block

                ctx.loop_info.pop();
            }

            TirStmtKind::ForOf {
                binding_local,
                binding_type,
                is_mut: _,
                iterable,
                iterable_type,
                body,
            } => {
                // For-of loop structure:
                // block $exit        ; break target
                //   loop $loop       ; for loop header
                //     block $body    ; continue target
                //       ;; Check: counter < array.used
                //       ;; Get element: array.repr[counter]
                //       ;; body
                //     end
                //     ;; Increment counter
                //     br $loop
                //   end
                // end
                //
                // From inside body:
                // - continue: br 0 (to end of $body, then counter++, then br $loop)
                // - break: br 2 (to $exit)

                // Get the raw array type index and Array struct type index for array.get
                let (raw_array_type_idx, array_struct_type_idx) =
                    if let Some(element_type) = type_table.as_array(*iterable_type) {
                        let raw_idx = self
                            .array_types
                            .get(&element_type)
                            .copied()
                            .unwrap_or(self.string_array_type_idx);
                        let struct_idx = *self
                            .array_struct_types
                            .get(&element_type)
                            .expect("Array struct type should be registered");
                        (raw_idx, struct_idx)
                    } else {
                        (self.string_array_type_idx, 0) // shouldn't happen
                    };

                // Get ValType for temporary locals (pre-allocated by preallocate_assert_locals_from_stmt)
                let _ = binding_type; // binding_local is pre-allocated by resolver
                let array_valtype = self.type_id_to_valtype(type_table, *iterable_type);

                // Get pre-allocated for-of temporary locals (unique names for nested loops)
                let for_of_id = ctx.next_for_of_id();
                let array_local =
                    ctx.alloc_local(&format!("__for_of_array_{for_of_id}"), array_valtype);
                let counter_local =
                    ctx.alloc_local(&format!("__for_of_counter_{for_of_id}"), ValType::I32);

                // Evaluate the iterable and store in array_local
                self.generate_expr(func, iterable, type_table, ctx, builder);
                func.instruction(&Instruction::LocalSet(array_local));

                // Initialize counter to 0
                func.instruction(&Instruction::I32Const(0));
                func.instruction(&Instruction::LocalSet(counter_local));

                // Push loop context: break_offset=2 (same as For)
                ctx.loop_info.push((0, 2));

                // block $exit
                func.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));
                // loop $loop
                func.instruction(&Instruction::Loop(wasm_encoder::BlockType::Empty));
                // block $body
                func.instruction(&Instruction::Block(wasm_encoder::BlockType::Empty));

                // Check: counter < array.used (field 1)
                func.instruction(&Instruction::LocalGet(counter_local));
                func.instruction(&Instruction::LocalGet(array_local));
                func.instruction(&Instruction::StructGet {
                    struct_type_index: array_struct_type_idx,
                    field_index: 1, // used is field 1
                });
                func.instruction(&Instruction::I32LtU);
                func.instruction(&Instruction::I32Eqz);
                // If counter >= length, exit (br 2 to $exit from inside $body block)
                func.instruction(&Instruction::BrIf(2));

                // Get element: array.repr[counter] and store in binding_local
                func.instruction(&Instruction::LocalGet(array_local));
                func.instruction(&Instruction::StructGet {
                    struct_type_index: array_struct_type_idx,
                    field_index: 0, // repr is field 0
                });
                func.instruction(&Instruction::LocalGet(counter_local));
                func.instruction(&Instruction::ArrayGet(raw_array_type_idx));
                // Store in the binding local (apply offset for closure functions)
                let adjusted_binding = *binding_local + ctx.local_index_offset;
                func.instruction(&Instruction::LocalSet(adjusted_binding));

                // Generate body
                self.generate_block(func, body, type_table, ctx, builder);

                // End $body block
                func.instruction(&Instruction::End);

                // Increment counter
                func.instruction(&Instruction::LocalGet(counter_local));
                func.instruction(&Instruction::I32Const(1));
                func.instruction(&Instruction::I32Add);
                func.instruction(&Instruction::LocalSet(counter_local));

                // Branch back to loop
                func.instruction(&Instruction::Br(0));

                // End $loop
                func.instruction(&Instruction::End);
                // End $exit block
                func.instruction(&Instruction::End);

                ctx.loop_info.pop();
            }

            TirStmtKind::Break => {
                if let Some((extra, break_offset)) = ctx.loop_info.last() {
                    // Break to outer block: break_offset + extra_depth
                    func.instruction(&Instruction::Br(break_offset + extra));
                } else {
                    // No enclosing loop - this should have been caught earlier
                    panic!("break outside of loop");
                }
            }

            TirStmtKind::Continue => {
                if let Some((extra, _)) = ctx.loop_info.last() {
                    // Continue to loop/body block: extra_depth
                    func.instruction(&Instruction::Br(*extra));
                } else {
                    // No enclosing loop - this should have been caught earlier
                    panic!("continue outside of loop");
                }
            }

            TirStmtKind::LabeledBlock { block, .. } => {
                // Generate a simple block - the label is for future use (break/continue)
                // For now, just generate the block contents in sequence
                self.generate_block(func, block, type_table, ctx, builder);
            }

            TirStmtKind::IfPattern {
                scrutinee,
                pattern,
                then_block,
                else_block,
            } => {
                self.generate_if_pattern(
                    func, scrutinee, pattern, then_block, else_block, type_table, ctx, builder,
                );
            }
        }
    }

    /// Generate code for if-pattern statement: `if Some(x) = expr { ... }`
    fn generate_if_pattern(
        &self,
        func: &mut Function,
        scrutinee: &TirExpr,
        pattern: &TirPattern,
        then_block: &TirBlock,
        else_block: &Option<TirBlock>,
        type_table: &TypeTable,
        ctx: &mut FunctionContext,
        builder: &CoreModuleBuilder,
    ) {
        // Generate the scrutinee expression
        self.generate_expr(func, scrutinee, type_table, ctx, builder);

        // Get the scrutinee type to determine how to match
        let scrutinee_type = type_table.get(scrutinee.type_id).clone();

        match (&scrutinee_type, pattern) {
            // Option<T> with Some(x) pattern - check for non-null and bind
            (
                ResolvedType::Option(inner_type),
                TirPattern::Variant {
                    variant_name,
                    bindings,
                    ..
                },
            ) if variant_name == "Some" => {
                // Stack: [option_value]
                // Store scrutinee in a temp local
                let option_valtype = self.type_id_to_valtype(type_table, scrutinee.type_id);
                let scrutinee_local = ctx.alloc_local("__if_pattern_scrutinee", option_valtype);
                func.instruction(&Instruction::LocalSet(scrutinee_local));

                // Generate: if (ref.is_null scrutinee) { else_block } else { then_block }
                // But wasm if expects condition to be true for then branch, so we flip
                func.instruction(&Instruction::LocalGet(scrutinee_local));
                func.instruction(&Instruction::RefIsNull);
                func.instruction(&Instruction::I32Eqz); // NOT: true if NOT null (Some)

                func.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
                if let Some((extra, _)) = ctx.loop_info.last_mut() {
                    *extra += 1;
                }

                // Then block: pattern matches (value is Some)
                // Bind the inner value to the pattern binding
                if let Some(TirPattern::Binding { local_index, .. }) = bindings.first() {
                    // Get the inner value (the non-null reference)
                    func.instruction(&Instruction::LocalGet(scrutinee_local));
                    func.instruction(&Instruction::RefAsNonNull);

                    // Store in the binding local with proper offset
                    let adjusted_index = *local_index + ctx.local_index_offset;
                    func.instruction(&Instruction::LocalSet(adjusted_index));
                }

                // Generate then block body
                self.generate_block(func, then_block, type_table, ctx, builder);

                // Else block (if any)
                if let Some(else_blk) = else_block {
                    func.instruction(&Instruction::Else);
                    self.generate_block(func, else_blk, type_table, ctx, builder);
                }

                func.instruction(&Instruction::End);
                if let Some((extra, _)) = ctx.loop_info.last_mut() {
                    *extra -= 1;
                }
            }

            // Option<T> with None pattern - check for null
            (ResolvedType::Option(_), TirPattern::Variant { variant_name, .. })
                if variant_name == "None" =>
            {
                // Stack: [option_value]
                // Check if null (None)
                func.instruction(&Instruction::RefIsNull); // true if null (None)

                func.instruction(&Instruction::If(wasm_encoder::BlockType::Empty));
                if let Some((extra, _)) = ctx.loop_info.last_mut() {
                    *extra += 1;
                }

                // Then block: pattern matches (value is None)
                self.generate_block(func, then_block, type_table, ctx, builder);

                // Else block (if any)
                if let Some(else_blk) = else_block {
                    func.instruction(&Instruction::Else);
                    self.generate_block(func, else_blk, type_table, ctx, builder);
                }

                func.instruction(&Instruction::End);
                if let Some((extra, _)) = ctx.loop_info.last_mut() {
                    *extra -= 1;
                }
            }

            // Unsupported pattern
            _ => {
                panic!(
                    "Unsupported if-pattern: {pattern:?} on type {scrutinee_type:?}"
                );
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

        // Collect closure locals from the function body (locals that store closure values)
        let closure_locals: HashMap<u32, u32> = if let Some(body) = &tir_func.body {
            Self::find_closure_locals(body)
        } else {
            HashMap::new()
        };

        // Store closure_locals in func_ctx for use during IndirectCall codegen
        func_ctx.local_closure_ids = closure_locals.clone();

        // Pre-allocate locals from TIR (skip params which are already added)
        for (i, &local_type_id) in tir_func.local_types.iter().enumerate() {
            let local_idx = i as u32;
            // Skip if it's a param (already added)
            if local_idx < tir_func.params.len() as u32 {
                continue;
            }

            // Check if this local stores a closure (use closure struct type)
            let local_type = if let Some(&closure_id) = closure_locals.get(&local_idx) {
                // Use the closure struct type for this local
                let pending = self.pending_closures.borrow();
                if let Some(closure_info) = pending.get(closure_id as usize) {
                    ValType::Ref(RefType {
                        nullable: true,
                        heap_type: HeapType::Concrete(closure_info.closure_struct_type_idx),
                    })
                } else {
                    self.type_id_to_valtype(type_table, local_type_id)
                }
            // For address-taken primitive locals, use box type instead
            } else if func_ctx.address_taken_locals.contains(&local_idx) {
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

        // Pre-allocate locals for assert statements
        if let Some(body) = &tir_func.body {
            let string_array_type = builder.type_idx("string-array");
            self.preallocate_assert_locals(body, type_table, &mut func_ctx, string_array_type);
        }

        // Pre-allocate locals for value copy operations (struct, array, tuple)
        if let Some(body) = &tir_func.body {
            self.preallocate_value_copy_locals(body, type_table, &mut func_ctx);
        }

        // Pre-allocate scratch locals for async effect handling (only if needed)
        if let Some(body) = &tir_func.body
            && Self::needs_async_scratch_locals(body)
        {
            Self::preallocate_async_scratch_locals(&mut func_ctx);
        }

        // Pre-allocate scratch locals for Environment calls (only if needed)
        if let Some(body) = &tir_func.body
            && Self::needs_environment_scratch_locals(body)
        {
            Self::preallocate_environment_scratch_locals(&mut func_ctx);
        }

        // Pre-allocate locals for closure calls
        if let Some(body) = &tir_func.body {
            self.preallocate_closure_call_locals(body, type_table, &mut func_ctx);
        }

        // Pre-allocate locals for array append operations
        if let Some(body) = &tir_func.body {
            self.preallocate_array_append_locals(body, type_table, &mut func_ctx);
        }

        // Pre-allocate locals for IfPattern statements
        if let Some(body) = &tir_func.body {
            self.preallocate_if_pattern_locals(body, type_table, &mut func_ctx);
        }

        // Reset for-of counter so code generation uses the same indices as pre-allocation
        func_ctx.reset_for_of_counter();

        // Generate the function code
        let mut wasm_func = Function::new(func_ctx.get_local_decls());

        // Generate body
        if let Some(body) = &tir_func.body {
            self.generate_block(&mut wasm_func, body, type_table, &mut func_ctx, builder);
        }

        // Add implicit return if needed
        if tir_func.return_type == TypeTable::UNIT {
            // Unit return - no value needed
        }
        wasm_func.instruction(&Instruction::End);

        // Return function and collected branch hints
        let branch_hints = func_ctx.branch_hints;
        (wasm_func, branch_hints)
    }

    /// Generate a closure implementation function.
    ///
    /// The closure function has the environment struct as its first parameter,
    /// followed by the regular closure parameters.
    /// The env parameter uses generic (ref struct) to allow canonical typing.
    /// Captured variables are accessed via ref.cast + struct.get on the environment.
    fn generate_closure_function(
        &self,
        closure_info: &ClosureInfo,
        type_table: &TypeTable,
        builder: &CoreModuleBuilder,
    ) -> Function {
        // Create function context with env as first param, then regular params
        let param_count = 1 + closure_info.params.len() as u32; // env + params
        let mut func_ctx = FunctionContext::new(param_count);

        // Add env parameter (index 0) - uses generic (ref struct) for canonical typing
        let env_type = ValType::Ref(RefType {
            nullable: false,
            heap_type: HeapType::Abstract {
                shared: false,
                ty: AbstractHeapType::Struct,
            },
        });
        func_ctx.add_param("$env", env_type);

        // Add regular parameters
        for (name, type_id) in &closure_info.params {
            let param_type = self.type_id_to_valtype(type_table, *type_id);
            func_ctx.add_param(name, param_type);
        }

        // Set return type
        if closure_info.return_type != TypeTable::UNIT
            && closure_info.return_type != TypeTable::NEVER
        {
            func_ctx.set_return_type(self.type_id_to_valtype(type_table, closure_info.return_type));
        }

        // Store closure info in context for capture access
        func_ctx.set_closure_info(closure_info.env_type_idx, &closure_info.captures);

        // Pre-allocate locals from block body if present
        if let TirExprKind::Block(ref block) = closure_info.body.kind {
            self.preallocate_locals_from_block(block, type_table, &mut func_ctx);
        }

        // Generate the function code
        let mut wasm_func = Function::new(func_ctx.get_local_decls());

        // Generate closure body
        self.generate_expr(
            &mut wasm_func,
            &closure_info.body,
            type_table,
            &mut func_ctx,
            builder,
        );

        // Add implicit return handling
        if closure_info.return_type == TypeTable::UNIT {
            // Drop the unit value if any
        }

        wasm_func.instruction(&Instruction::End);

        wasm_func
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

        // Copy address-taken locals from TIR
        func_ctx.address_taken_locals = tir_func.address_taken_locals.clone();

        // Add parameters to context
        for param in &tir_func.params {
            let param_type = self.type_id_to_valtype(type_table, param.type_id);
            func_ctx.add_param(&param.name, param_type);
        }

        // Collect closure locals from the function body (locals that store closure values)
        let closure_locals: HashMap<u32, u32> = if let Some(body) = &tir_func.body {
            Self::find_closure_locals(body)
        } else {
            HashMap::new()
        };

        // Store closure_locals in func_ctx for use during IndirectCall codegen
        func_ctx.local_closure_ids = closure_locals.clone();

        // Pre-allocate locals from TIR (skip params which are already added)
        for (i, &local_type_id) in tir_func.local_types.iter().enumerate() {
            let local_idx = i as u32;
            // Skip if it's a param (already added)
            if local_idx < tir_func.params.len() as u32 {
                continue;
            }

            // Check if this local stores a closure (use closure struct type)
            let local_type = if let Some(&closure_id) = closure_locals.get(&local_idx) {
                // Use the closure struct type for this local
                let pending = self.pending_closures.borrow();
                if let Some(closure_info) = pending.get(closure_id as usize) {
                    ValType::Ref(RefType {
                        nullable: true,
                        heap_type: HeapType::Concrete(closure_info.closure_struct_type_idx),
                    })
                } else {
                    self.type_id_to_valtype(type_table, local_type_id)
                }
            // For address-taken primitive locals, use box type instead
            } else if func_ctx.address_taken_locals.contains(&local_idx) {
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

        // Pre-allocate locals for assert statements
        if let Some(body) = &tir_func.body {
            let string_array_type = builder.type_idx("string-array");
            self.preallocate_assert_locals(body, type_table, &mut func_ctx, string_array_type);
        }

        // Pre-allocate locals for value copy operations (struct, array, tuple)
        if let Some(body) = &tir_func.body {
            self.preallocate_value_copy_locals(body, type_table, &mut func_ctx);
        }

        // Pre-allocate scratch locals for async effect handling (only if needed)
        if let Some(body) = &tir_func.body
            && Self::needs_async_scratch_locals(body)
        {
            Self::preallocate_async_scratch_locals(&mut func_ctx);
        }

        // Pre-allocate scratch locals for Environment calls (only if needed)
        if let Some(body) = &tir_func.body
            && Self::needs_environment_scratch_locals(body)
        {
            Self::preallocate_environment_scratch_locals(&mut func_ctx);
        }

        // Pre-allocate locals for closure calls
        if let Some(body) = &tir_func.body {
            self.preallocate_closure_call_locals(body, type_table, &mut func_ctx);
        }

        // Pre-allocate locals for array append operations
        if let Some(body) = &tir_func.body {
            self.preallocate_array_append_locals(body, type_table, &mut func_ctx);
        }

        // Pre-allocate locals for IfPattern statements
        if let Some(body) = &tir_func.body {
            self.preallocate_if_pattern_locals(body, type_table, &mut func_ctx);
        }

        // Reset for-of counter so code generation uses the same indices as pre-allocation
        func_ctx.reset_for_of_counter();

        let mut wasm_func = Function::new(func_ctx.get_local_decls());

        // Generate body
        if let Some(body) = &tir_func.body {
            self.generate_block(&mut wasm_func, body, type_table, &mut func_ctx, builder);
        }

        // Call task.return to complete the async task (0 = ok)
        let task_return_idx = builder.func_idx("task-return");
        wasm_func.instruction(&Instruction::I32Const(0));
        wasm_func.instruction(&Instruction::Call(task_return_idx));
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
            && let Some(builtin_info) = self.builtin_registry.get(func_name)
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
            if let Some(wasi_local_name) = self.wasi_registry.resolve(&effect_qualified_name)
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
                for arg in args {
                    self.generate_expr(func, arg, type_table, ctx, builder);
                }
                ctx.set_branch_hint(true);
            }
            "builtin::unlikely" => {
                // Pass through the argument and set branch hint for the next branch
                for arg in args {
                    self.generate_expr(func, arg, type_table, ctx, builder);
                }
                ctx.set_branch_hint(false);
            }
            "builtin::unreachable" => {
                func.instruction(&Instruction::Unreachable);
            }
            "builtin::effect_wait" => {
                self.generate_effect_wait(func, ctx, builder);
            }
            "builtin::array_len" => {
                for arg in args {
                    self.generate_expr(func, arg, type_table, ctx, builder);
                }
                func.instruction(&Instruction::ArrayLen);
            }
            "builtin::array_get_u8" => {
                for arg in args {
                    self.generate_expr(func, arg, type_table, ctx, builder);
                }
                func.instruction(&Instruction::ArrayGetU(self.string_array_type_idx));
            }
            "builtin::array_set_u8" => {
                for arg in args {
                    self.generate_expr(func, arg, type_table, ctx, builder);
                }
                func.instruction(&Instruction::ArraySet(self.string_array_type_idx));
            }
            "builtin::string_new" => {
                if let Some(len_arg) = args.first() {
                    self.generate_expr(func, len_arg, type_table, ctx, builder);
                    func.instruction(&Instruction::ArrayNewDefault(self.string_array_type_idx));
                    let string_struct_info = self
                        .lookup_struct_type("String", &string_module_path())
                        .expect("String struct not found");
                    func.instruction(&Instruction::StructNew(string_struct_info.type_idx));
                }
            }
            "builtin::memory_store8" => {
                for arg in args {
                    self.generate_expr(func, arg, type_table, ctx, builder);
                }
                func.instruction(&Instruction::I32Store8(MemArg {
                    offset: 0,
                    align: 0,
                    memory_index: 0,
                }));
            }
            "builtin::memory_load8_u" => {
                for arg in args {
                    self.generate_expr(func, arg, type_table, ctx, builder);
                }
                func.instruction(&Instruction::I32Load8U(MemArg {
                    offset: 0,
                    align: 0,
                    memory_index: 0,
                }));
            }
            "builtin::memory_load32" => {
                for arg in args {
                    self.generate_expr(func, arg, type_table, ctx, builder);
                }
                func.instruction(&Instruction::I32Load(MemArg {
                    offset: 0,
                    align: 2,
                    memory_index: 0,
                }));
            }
            "builtin::array_new_string" => {
                for arg in args {
                    self.generate_expr(func, arg, type_table, ctx, builder);
                }
                let string_type_id = type_table
                    .find_struct_type("String", &string_module_path())
                    .expect("String struct should be defined in core/prelude");
                let array_of_string_type = *self
                    .array_types
                    .get(&string_type_id)
                    .expect("Array<String> raw array type should be registered");
                func.instruction(&Instruction::ArrayNewDefault(array_of_string_type));
            }
            "builtin::array_wrap_string" => {
                for arg in args {
                    self.generate_expr(func, arg, type_table, ctx, builder);
                }
                let string_type_id = type_table
                    .find_struct_type("String", &string_module_path())
                    .expect("String struct should be defined in core/prelude");
                let array_struct_type = *self
                    .array_struct_types
                    .get(&string_type_id)
                    .expect("Array<String> struct type should be registered");
                func.instruction(&Instruction::StructNew(array_struct_type));
            }
            "builtin::array_set_string" => {
                for arg in args {
                    self.generate_expr(func, arg, type_table, ctx, builder);
                }
                let string_type_id = type_table
                    .find_struct_type("String", &string_module_path())
                    .expect("String struct should be defined in core/prelude");
                let array_of_string_type = *self
                    .array_types
                    .get(&string_type_id)
                    .expect("Array<String> type should be registered");
                func.instruction(&Instruction::ArraySet(array_of_string_type));
            }
            "builtin::array_get_string" => {
                for arg in args {
                    self.generate_expr(func, arg, type_table, ctx, builder);
                }
                let string_type_id = type_table
                    .find_struct_type("String", &string_module_path())
                    .expect("String struct should be defined in core/prelude");
                let array_of_string_type = *self
                    .array_types
                    .get(&string_type_id)
                    .expect("Array<String> type should be registered");
                func.instruction(&Instruction::ArrayGet(array_of_string_type));
            }
            "builtin::array_new" => {
                if let ResolvedType::BuiltinArray(element_type) = type_table.get(expr.type_id) {
                    let array_type_idx = *self
                        .array_types
                        .get(element_type)
                        .expect("Array type should be registered for array_new");
                    for arg in args {
                        self.generate_expr(func, arg, type_table, ctx, builder);
                    }
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
                        let array_type_idx = *self
                            .array_types
                            .get(element_type)
                            .expect("Array type should be registered for array_get");
                        for arg in args {
                            self.generate_expr(func, arg, type_table, ctx, builder);
                        }
                        func.instruction(&Instruction::ArrayGet(array_type_idx));
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
                        let array_type_idx = *self
                            .array_types
                            .get(element_type)
                            .expect("Array type should be registered for array_set");
                        for arg in args {
                            self.generate_expr(func, arg, type_table, ctx, builder);
                        }
                        func.instruction(&Instruction::ArraySet(array_type_idx));
                    } else {
                        panic!("array_set first argument must be builtin::array<T>");
                    }
                }
            }
            "builtin::array_copy" => {
                if let Some(dst_arg) = args.first() {
                    if let ResolvedType::BuiltinArray(element_type) =
                        type_table.get(dst_arg.type_id)
                    {
                        let array_type_idx = *self
                            .array_types
                            .get(element_type)
                            .expect("Array type should be registered for array_copy");
                        for arg in args {
                            self.generate_expr(func, arg, type_table, ctx, builder);
                        }
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
                        let array_type_idx = *self
                            .array_types
                            .get(element_type)
                            .expect("Array type should be registered for array_fill");
                        for arg in args {
                            self.generate_expr(func, arg, type_table, ctx, builder);
                        }
                        func.instruction(&Instruction::ArrayFill(array_type_idx));
                    } else {
                        panic!("array_fill first argument must be builtin::array<T>");
                    }
                }
            }
            "builtin::i32_and" => {
                for arg in args {
                    self.generate_expr(func, arg, type_table, ctx, builder);
                }
                func.instruction(&Instruction::I32And);
            }
            "builtin::i32_eqz" => {
                for arg in args {
                    self.generate_expr(func, arg, type_table, ctx, builder);
                }
                func.instruction(&Instruction::I32Eqz);
            }
            "builtin::call_indirect_stdout_write_via_stream" => {
                for arg in args {
                    self.generate_expr(func, arg, type_table, ctx, builder);
                }
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
                for arg in args {
                    self.generate_expr(func, arg, type_table, ctx, builder);
                }
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
            | "builtin::task_return"
            | "builtin::waitable_set_new"
            | "builtin::waitable_join"
            | "builtin::waitable_set_wait"
            | "builtin::subtask_drop" => {
                for arg in args {
                    self.generate_expr(func, arg, type_table, ctx, builder);
                }
                // Look up the canonical name from the builtin registry
                let func_name = builtin_name.strip_prefix("builtin::").unwrap();
                let builtin_info = self
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
                    for arg in args {
                        self.generate_expr(func, arg, type_table, ctx, builder);
                    }
                }
                true
            }
            "Err" => {
                let is_unit_payload = args.is_empty()
                    || (args.len() == 1 && matches!(&args[0].kind, TirExprKind::Unit));
                if is_unit_payload {
                    func.instruction(&Instruction::I32Const(1));
                } else {
                    for arg in args {
                        self.generate_expr(func, arg, type_table, ctx, builder);
                    }
                }
                true
            }
            "Some" => {
                for arg in args {
                    self.generate_expr(func, arg, type_table, ctx, builder);
                }
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

    /// Pre-allocate scratch locals for async effect handling
    ///
    /// These are needed for `builtin::call_indirect_stdout/stderr_write_via_stream`
    /// and `builtin::effect_wait` (used by ambient logging functions like `log_stdout`).
    /// Only allocate when the function actually uses these builtins.
    fn preallocate_async_scratch_locals(ctx: &mut FunctionContext) {
        // Scratch locals for write_via_stream async handling
        ctx.alloc_local("__subtask", ValType::I32);
        ctx.alloc_local("__waitable_set", ValType::I32);
    }

    /// Pre-allocate scratch locals for Environment calls
    ///
    /// Environment calls (`get_arguments`, `get_environment`, `get_initial_cwd`) need
    /// a local to hold the outptr for CM ABI conversion.
    fn preallocate_environment_scratch_locals(ctx: &mut FunctionContext) {
        ctx.alloc_local("__cm_outptr", ValType::I32);
    }

    /// Check if a function body uses Environment calls that need scratch locals.
    fn needs_environment_scratch_locals(block: &TirBlock) -> bool {
        for stmt in &block.stmts {
            if Self::stmt_needs_environment_scratch_locals(stmt) {
                return true;
            }
        }
        false
    }

    fn stmt_needs_environment_scratch_locals(stmt: &TirStmt) -> bool {
        match &stmt.kind {
            TirStmtKind::Let { value, .. } | TirStmtKind::Expr(value) => {
                Self::expr_needs_environment_scratch_locals(value)
            }
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
                ..
            } => {
                Self::expr_needs_environment_scratch_locals(condition)
                    || Self::needs_environment_scratch_locals(then_block)
                    || else_block
                        .as_ref()
                        .is_some_and(Self::needs_environment_scratch_locals)
            }
            TirStmtKind::While { condition, body } => {
                Self::expr_needs_environment_scratch_locals(condition)
                    || Self::needs_environment_scratch_locals(body)
            }
            TirStmtKind::For {
                condition,
                update,
                body,
            } => {
                condition
                    .as_ref()
                    .is_some_and(Self::expr_needs_environment_scratch_locals)
                    || update
                        .as_ref()
                        .is_some_and(Self::expr_needs_environment_scratch_locals)
                    || Self::needs_environment_scratch_locals(body)
            }
            TirStmtKind::ForOf { iterable, body, .. } => {
                Self::expr_needs_environment_scratch_locals(iterable)
                    || Self::needs_environment_scratch_locals(body)
            }
            TirStmtKind::Loop { body } => Self::needs_environment_scratch_locals(body),
            TirStmtKind::Return { value: Some(expr) } => {
                Self::expr_needs_environment_scratch_locals(expr)
            }
            _ => false,
        }
    }

    fn expr_needs_environment_scratch_locals(expr: &TirExpr) -> bool {
        match &expr.kind {
            TirExprKind::Call { func, args, .. } => {
                let module_path = func.module_path();
                let func_name = func.name();
                // Check for Environment calls that need scratch locals
                if module_path.len() == 1
                    && module_path[0] == "Environment"
                    && matches!(
                        func_name.as_str(),
                        "get_arguments" | "get_environment" | "get_initial_cwd"
                    )
                {
                    return true;
                }
                args.iter().any(Self::expr_needs_environment_scratch_locals)
            }
            TirExprKind::MethodCall { receiver, args, .. } => {
                Self::expr_needs_environment_scratch_locals(receiver)
                    || args.iter().any(Self::expr_needs_environment_scratch_locals)
            }
            TirExprKind::Binary { left, right, .. } => {
                Self::expr_needs_environment_scratch_locals(left)
                    || Self::expr_needs_environment_scratch_locals(right)
            }
            TirExprKind::Unary { expr, .. } => Self::expr_needs_environment_scratch_locals(expr),
            TirExprKind::Assign { target, value } => {
                Self::expr_needs_environment_scratch_locals(target)
                    || Self::expr_needs_environment_scratch_locals(value)
            }
            TirExprKind::Cast { expr, .. } => Self::expr_needs_environment_scratch_locals(expr),
            TirExprKind::EffectCall { args, .. } | TirExprKind::StaticCall { args, .. } => {
                args.iter().any(Self::expr_needs_environment_scratch_locals)
            }
            TirExprKind::FieldAccess { expr, .. } => {
                Self::expr_needs_environment_scratch_locals(expr)
            }
            TirExprKind::Index { expr, index } => {
                Self::expr_needs_environment_scratch_locals(expr)
                    || Self::expr_needs_environment_scratch_locals(index)
            }
            TirExprKind::Block(block) => Self::needs_environment_scratch_locals(block),
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                Self::expr_needs_environment_scratch_locals(condition)
                    || Self::needs_environment_scratch_locals(then_branch)
                    || else_branch
                        .as_ref()
                        .is_some_and(Self::needs_environment_scratch_locals)
            }
            TirExprKind::StructLiteral { fields, .. } => fields
                .iter()
                .any(|f| Self::expr_needs_environment_scratch_locals(&f.value)),
            TirExprKind::ArrayLiteral { elements } | TirExprKind::TupleLiteral { elements } => {
                elements
                    .iter()
                    .any(Self::expr_needs_environment_scratch_locals)
            }
            TirExprKind::Closure { body, .. } => Self::expr_needs_environment_scratch_locals(body),
            TirExprKind::IndirectCall { callee, args } => {
                Self::expr_needs_environment_scratch_locals(callee)
                    || args.iter().any(Self::expr_needs_environment_scratch_locals)
            }
            TirExprKind::Match { expr, arms } => {
                Self::expr_needs_environment_scratch_locals(expr)
                    || arms
                        .iter()
                        .any(|arm| Self::expr_needs_environment_scratch_locals(&arm.body))
            }
            // Leaf nodes
            _ => false,
        }
    }

    /// Check if a function body uses async builtins that need scratch locals.
    ///
    /// Returns true if the body calls:
    /// - `builtin::call_indirect_stdout_write_via_stream`
    /// - `builtin::call_indirect_stderr_write_via_stream`
    /// - `builtin::effect_wait`
    fn needs_async_scratch_locals(block: &TirBlock) -> bool {
        for stmt in &block.stmts {
            if Self::stmt_needs_async_scratch_locals(stmt) {
                return true;
            }
        }
        false
    }

    fn stmt_needs_async_scratch_locals(stmt: &TirStmt) -> bool {
        match &stmt.kind {
            TirStmtKind::Let { value, .. } | TirStmtKind::Expr(value) => {
                Self::expr_needs_async_scratch_locals(value)
            }
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
                ..
            } => {
                Self::expr_needs_async_scratch_locals(condition)
                    || Self::needs_async_scratch_locals(then_block)
                    || else_block
                        .as_ref()
                        .is_some_and(Self::needs_async_scratch_locals)
            }
            TirStmtKind::While { condition, body } => {
                Self::expr_needs_async_scratch_locals(condition)
                    || Self::needs_async_scratch_locals(body)
            }
            TirStmtKind::For {
                condition,
                update,
                body,
            } => {
                condition
                    .as_ref()
                    .is_some_and(Self::expr_needs_async_scratch_locals)
                    || update
                        .as_ref()
                        .is_some_and(Self::expr_needs_async_scratch_locals)
                    || Self::needs_async_scratch_locals(body)
            }
            TirStmtKind::ForOf { iterable, body, .. } => {
                Self::expr_needs_async_scratch_locals(iterable)
                    || Self::needs_async_scratch_locals(body)
            }
            TirStmtKind::Loop { body } => Self::needs_async_scratch_locals(body),
            TirStmtKind::Return { value: Some(expr) } => {
                Self::expr_needs_async_scratch_locals(expr)
            }
            _ => false,
        }
    }

    fn expr_needs_async_scratch_locals(expr: &TirExpr) -> bool {
        match &expr.kind {
            TirExprKind::Call { func, args, .. } => {
                // Check if this is a builtin call that needs async scratch locals
                if let Some(builtin) = func.builtin_name()
                    && matches!(
                        builtin.as_str(),
                        "builtin::call_indirect_stdout_write_via_stream"
                            | "builtin::call_indirect_stderr_write_via_stream"
                            | "builtin::effect_wait"
                    )
                {
                    return true;
                }

                // Check if this is a direct WASI effect call (Stdout/Stderr::write_via_stream)
                let module_path = func.module_path();
                let func_name = func.name();
                if module_path.len() == 1
                    && (module_path[0] == "Stdout" || module_path[0] == "Stderr")
                    && func_name == "write_via_stream"
                {
                    return true;
                }

                // Check args recursively
                args.iter().any(Self::expr_needs_async_scratch_locals)
            }
            TirExprKind::MethodCall { receiver, args, .. } => {
                Self::expr_needs_async_scratch_locals(receiver)
                    || args.iter().any(Self::expr_needs_async_scratch_locals)
            }
            TirExprKind::Binary { left, right, .. } => {
                Self::expr_needs_async_scratch_locals(left)
                    || Self::expr_needs_async_scratch_locals(right)
            }
            TirExprKind::Unary { expr, .. } => Self::expr_needs_async_scratch_locals(expr),
            TirExprKind::Assign { target, value } => {
                Self::expr_needs_async_scratch_locals(target)
                    || Self::expr_needs_async_scratch_locals(value)
            }
            TirExprKind::Cast { expr, .. } => Self::expr_needs_async_scratch_locals(expr),
            TirExprKind::EffectCall {
                effect_name,
                op_name,
                args,
            } => {
                // Stdout/Stderr write_via_stream effect calls need async scratch locals
                if (effect_name == "Stdout" || effect_name == "Stderr")
                    && op_name == "write_via_stream"
                {
                    return true;
                }
                args.iter().any(Self::expr_needs_async_scratch_locals)
            }
            TirExprKind::StaticCall { args, .. } => {
                args.iter().any(Self::expr_needs_async_scratch_locals)
            }
            TirExprKind::FieldAccess { expr, .. } => Self::expr_needs_async_scratch_locals(expr),
            TirExprKind::Block(block) => Self::needs_async_scratch_locals(block),
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                Self::expr_needs_async_scratch_locals(condition)
                    || Self::needs_async_scratch_locals(then_branch)
                    || else_branch
                        .as_ref()
                        .is_some_and(Self::needs_async_scratch_locals)
            }
            TirExprKind::StructLiteral { fields, .. } => fields
                .iter()
                .any(|f| Self::expr_needs_async_scratch_locals(&f.value)),
            TirExprKind::ArrayLiteral { elements } | TirExprKind::TupleLiteral { elements } => {
                elements.iter().any(Self::expr_needs_async_scratch_locals)
            }
            TirExprKind::Closure { body, .. } => Self::expr_needs_async_scratch_locals(body),
            TirExprKind::IndirectCall { callee, args } => {
                Self::expr_needs_async_scratch_locals(callee)
                    || args.iter().any(Self::expr_needs_async_scratch_locals)
            }
            TirExprKind::Index { expr, index } => {
                Self::expr_needs_async_scratch_locals(expr)
                    || Self::expr_needs_async_scratch_locals(index)
            }
            TirExprKind::Match { expr, arms } => {
                Self::expr_needs_async_scratch_locals(expr)
                    || arms
                        .iter()
                        .any(|arm| Self::expr_needs_async_scratch_locals(&arm.body))
            }
            TirExprKind::OptionSome { value } => Self::expr_needs_async_scratch_locals(value),
            TirExprKind::VariantConstruct { fields, .. } => {
                fields.iter().any(Self::expr_needs_async_scratch_locals)
            }
            TirExprKind::Move { value } => Self::expr_needs_async_scratch_locals(value),
            // Leaf nodes - no calls
            TirExprKind::IntLiteral { .. }
            | TirExprKind::FloatLiteral { .. }
            | TirExprKind::BoolLiteral(_)
            | TirExprKind::CharLiteral(_)
            | TirExprKind::StringLiteral(_)
            | TirExprKind::Null
            | TirExprKind::Unit
            | TirExprKind::Local { .. }
            | TirExprKind::Global { .. }
            | TirExprKind::Capture { .. } => false,
        }
    }

    /// Pre-allocate locals for value copy operations (struct, array, tuple).
    /// This must be called before code generation to ensure copy locals are available.
    fn preallocate_value_copy_locals(
        &self,
        block: &TirBlock,
        type_table: &TypeTable,
        ctx: &mut FunctionContext,
    ) {
        let mut needed_types: std::collections::HashSet<TypeId> = std::collections::HashSet::new();
        self.collect_copy_types(block, type_table, &mut needed_types);

        for type_id in needed_types {
            match type_table.get(type_id) {
                ResolvedType::Struct { name, module_path } => {
                    if let Some(info) = self.lookup_struct_type(name, module_path) {
                        ctx.alloc_local(
                            &format!("__copy_source_{}", info.type_idx),
                            ValType::Ref(RefType {
                                nullable: true,
                                heap_type: HeapType::Concrete(info.type_idx),
                            }),
                        );
                    }
                }
                ResolvedType::Tuple(elements) => {
                    if let Some(type_idx) = self.get_tuple_type_idx(elements) {
                        ctx.alloc_local(
                            &format!("__copy_source_{type_idx}"),
                            ValType::Ref(RefType {
                                nullable: true,
                                heap_type: HeapType::Concrete(type_idx),
                            }),
                        );
                    }
                }
                ResolvedType::GenericInstance {
                    name, type_args, ..
                } if name == "Array" && type_args.len() == 1 => {
                    let elem_type = type_args[0];
                    if let Some(&raw_array_type_idx) = self.array_types.get(&elem_type) {
                        // Allocate locals for the Array struct wrapper
                        if let Some(&array_struct_type_idx) =
                            self.array_struct_types.get(&elem_type)
                        {
                            ctx.alloc_local(
                                &format!("__copy_array_struct_source_{raw_array_type_idx}"),
                                ValType::Ref(RefType {
                                    nullable: true,
                                    heap_type: HeapType::Concrete(array_struct_type_idx),
                                }),
                            );
                        }
                        // Allocate locals for raw array copy operations
                        ctx.alloc_local(
                            &format!("__copy_array_source_{raw_array_type_idx}"),
                            ValType::Ref(RefType {
                                nullable: true,
                                heap_type: HeapType::Concrete(raw_array_type_idx),
                            }),
                        );
                        ctx.alloc_local(
                            &format!("__copy_array_dest_{raw_array_type_idx}"),
                            ValType::Ref(RefType {
                                nullable: true,
                                heap_type: HeapType::Concrete(raw_array_type_idx),
                            }),
                        );
                        ctx.alloc_local(
                            &format!("__copy_array_counter_{raw_array_type_idx}"),
                            ValType::I32,
                        );
                        ctx.alloc_local(
                            &format!("__copy_array_len_{raw_array_type_idx}"),
                            ValType::I32,
                        );
                    }
                }
                ResolvedType::String => {
                    // String is now a struct, allocate struct copy local
                    if let Some(info) = self.lookup_struct_type("String", &string_module_path()) {
                        ctx.alloc_local(
                            &format!("__copy_source_{}", info.type_idx),
                            ValType::Ref(RefType {
                                nullable: true,
                                heap_type: HeapType::Concrete(info.type_idx),
                            }),
                        );
                    }
                }
                _ => {}
            }
        }
    }

    /// Collect all types that need value copy from a block
    fn collect_copy_types(
        &self,
        block: &TirBlock,
        type_table: &TypeTable,
        needed_types: &mut std::collections::HashSet<TypeId>,
    ) {
        for stmt in &block.stmts {
            self.collect_copy_types_from_stmt(stmt, type_table, needed_types);
        }
    }

    /// Collect copy types from a statement
    fn collect_copy_types_from_stmt(
        &self,
        stmt: &TirStmt,
        type_table: &TypeTable,
        needed_types: &mut std::collections::HashSet<TypeId>,
    ) {
        match &stmt.kind {
            TirStmtKind::Let { value, .. } => {
                self.collect_copy_types_from_expr(value, type_table, needed_types);
                if self.needs_value_copy(value.type_id, type_table) {
                    needed_types.insert(value.type_id);
                }
            }
            TirStmtKind::Expr(expr) => {
                self.collect_copy_types_from_expr(expr, type_table, needed_types);
            }
            TirStmtKind::While { condition, body } => {
                self.collect_copy_types_from_expr(condition, type_table, needed_types);
                self.collect_copy_types(body, type_table, needed_types);
            }
            TirStmtKind::For {
                condition,
                update,
                body,
            } => {
                if let Some(e) = condition {
                    self.collect_copy_types_from_expr(e, type_table, needed_types);
                }
                if let Some(e) = update {
                    self.collect_copy_types_from_expr(e, type_table, needed_types);
                }
                self.collect_copy_types(body, type_table, needed_types);
            }
            TirStmtKind::ForOf { iterable, body, .. } => {
                self.collect_copy_types_from_expr(iterable, type_table, needed_types);
                self.collect_copy_types(body, type_table, needed_types);
            }
            TirStmtKind::Loop { body } => {
                self.collect_copy_types(body, type_table, needed_types);
            }
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                self.collect_copy_types_from_expr(condition, type_table, needed_types);
                self.collect_copy_types(then_block, type_table, needed_types);
                if let Some(e) = else_block {
                    self.collect_copy_types(e, type_table, needed_types);
                }
            }
            TirStmtKind::Return { value: Some(expr) } => {
                self.collect_copy_types_from_expr(expr, type_table, needed_types);
            }
            _ => {}
        }
    }

    /// Collect copy types from an expression
    fn collect_copy_types_from_expr(
        &self,
        expr: &TirExpr,
        type_table: &TypeTable,
        needed_types: &mut std::collections::HashSet<TypeId>,
    ) {
        match &expr.kind {
            TirExprKind::Assign { target, value } => {
                self.collect_copy_types_from_expr(target, type_table, needed_types);
                self.collect_copy_types_from_expr(value, type_table, needed_types);
                // Check if assigning to a local variable with value type
                if matches!(target.kind, TirExprKind::Local { .. })
                    && self.needs_value_copy(value.type_id, type_table)
                {
                    needed_types.insert(value.type_id);
                }
            }
            TirExprKind::Binary { left, right, .. } => {
                self.collect_copy_types_from_expr(left, type_table, needed_types);
                self.collect_copy_types_from_expr(right, type_table, needed_types);
            }
            TirExprKind::Unary { expr, .. } => {
                self.collect_copy_types_from_expr(expr, type_table, needed_types);
            }
            TirExprKind::Call { args, .. } | TirExprKind::StaticCall { args, .. } => {
                for arg in args {
                    self.collect_copy_types_from_expr(arg, type_table, needed_types);
                }
            }
            TirExprKind::MethodCall { receiver, args, .. } => {
                self.collect_copy_types_from_expr(receiver, type_table, needed_types);
                for arg in args {
                    self.collect_copy_types_from_expr(arg, type_table, needed_types);
                }
            }
            TirExprKind::FieldAccess { expr, .. } => {
                self.collect_copy_types_from_expr(expr, type_table, needed_types);
            }
            TirExprKind::Index { expr, index } => {
                self.collect_copy_types_from_expr(expr, type_table, needed_types);
                self.collect_copy_types_from_expr(index, type_table, needed_types);
            }
            TirExprKind::ArrayLiteral { elements } => {
                for e in elements {
                    self.collect_copy_types_from_expr(e, type_table, needed_types);
                }
            }
            TirExprKind::StructLiteral { fields, .. } => {
                for field in fields {
                    self.collect_copy_types_from_expr(&field.value, type_table, needed_types);
                }
            }
            TirExprKind::TupleLiteral { elements } => {
                for e in elements {
                    self.collect_copy_types_from_expr(e, type_table, needed_types);
                }
            }
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.collect_copy_types_from_expr(condition, type_table, needed_types);
                self.collect_copy_types(then_branch, type_table, needed_types);
                if let Some(e) = else_branch {
                    self.collect_copy_types(e, type_table, needed_types);
                }
            }
            TirExprKind::Block(block) => {
                self.collect_copy_types(block, type_table, needed_types);
            }
            TirExprKind::Match { expr, arms } => {
                self.collect_copy_types_from_expr(expr, type_table, needed_types);
                for arm in arms {
                    self.collect_copy_types_from_expr(&arm.body, type_table, needed_types);
                }
            }
            TirExprKind::Cast { expr, .. } => {
                self.collect_copy_types_from_expr(expr, type_table, needed_types);
            }
            TirExprKind::Move { value } => {
                self.collect_copy_types_from_expr(value, type_table, needed_types);
            }
            _ => {}
        }
    }

    /// Pre-allocate locals for TIR assert statements in a block
    fn preallocate_assert_locals(
        &self,
        block: &TirBlock,
        type_table: &TypeTable,
        ctx: &mut FunctionContext,
        string_array_type: u32,
    ) {
        for stmt in &block.stmts {
            self.preallocate_assert_locals_from_stmt(stmt, type_table, ctx, string_array_type);
        }
    }

    /// Pre-allocate locals from a single TIR statement (recursively handles nested blocks)
    fn preallocate_assert_locals_from_stmt(
        &self,
        stmt: &TirStmt,
        type_table: &TypeTable,
        ctx: &mut FunctionContext,
        string_array_type: u32,
    ) {
        match &stmt.kind {
            TirStmtKind::While { body, .. } => {
                self.preallocate_assert_locals(body, type_table, ctx, string_array_type);
            }
            TirStmtKind::For { body, .. } => {
                self.preallocate_assert_locals(body, type_table, ctx, string_array_type);
            }
            TirStmtKind::ForOf {
                iterable_type,
                body,
                ..
            } => {
                // Pre-allocate for-of temporary locals with unique names for nested loops
                let for_of_id = ctx.next_for_of_id();
                let array_valtype = self.type_id_to_valtype(type_table, *iterable_type);
                ctx.alloc_local(&format!("__for_of_array_{for_of_id}"), array_valtype);
                ctx.alloc_local(&format!("__for_of_counter_{for_of_id}"), ValType::I32);
                // Recursively handle body
                self.preallocate_assert_locals(body, type_table, ctx, string_array_type);
            }
            TirStmtKind::Loop { body } => {
                self.preallocate_assert_locals(body, type_table, ctx, string_array_type);
            }
            TirStmtKind::If {
                then_block,
                else_block,
                ..
            } => {
                self.preallocate_assert_locals(then_block, type_table, ctx, string_array_type);
                if let Some(else_blk) = else_block {
                    self.preallocate_assert_locals(else_blk, type_table, ctx, string_array_type);
                }
            }
            TirStmtKind::IfPattern {
                then_block,
                else_block,
                ..
            } => {
                self.preallocate_assert_locals(then_block, type_table, ctx, string_array_type);
                if let Some(else_blk) = else_block {
                    self.preallocate_assert_locals(else_blk, type_table, ctx, string_array_type);
                }
            }
            _ => {}
        }
    }

    /// Pre-allocate locals for `IfPattern` statements in a block
    fn preallocate_if_pattern_locals(
        &self,
        block: &TirBlock,
        type_table: &TypeTable,
        ctx: &mut FunctionContext,
    ) {
        for stmt in &block.stmts {
            self.preallocate_if_pattern_locals_from_stmt(stmt, type_table, ctx);
        }
    }

    fn preallocate_if_pattern_locals_from_stmt(
        &self,
        stmt: &TirStmt,
        type_table: &TypeTable,
        ctx: &mut FunctionContext,
    ) {
        match &stmt.kind {
            TirStmtKind::IfPattern {
                scrutinee,
                then_block,
                else_block,
                ..
            } => {
                // Pre-allocate temp local for the scrutinee
                let scrutinee_type = type_table.get(scrutinee.type_id).clone();
                if let ResolvedType::Option(_) = scrutinee_type {
                    let option_valtype = self.type_id_to_valtype(type_table, scrutinee.type_id);
                    ctx.alloc_local("__if_pattern_scrutinee", option_valtype);
                }
                // Recursively handle nested blocks
                self.preallocate_if_pattern_locals(then_block, type_table, ctx);
                if let Some(else_blk) = else_block {
                    self.preallocate_if_pattern_locals(else_blk, type_table, ctx);
                }
            }
            TirStmtKind::If {
                then_block,
                else_block,
                ..
            } => {
                self.preallocate_if_pattern_locals(then_block, type_table, ctx);
                if let Some(else_blk) = else_block {
                    self.preallocate_if_pattern_locals(else_blk, type_table, ctx);
                }
            }
            TirStmtKind::While { body, .. }
            | TirStmtKind::For { body, .. }
            | TirStmtKind::Loop { body }
            | TirStmtKind::ForOf { body, .. }
            | TirStmtKind::LabeledBlock { block: body, .. } => {
                self.preallocate_if_pattern_locals(body, type_table, ctx);
            }
            _ => {}
        }
    }

    /// Pre-allocate locals for closure call expressions.
    /// This must be called before the Function is created.
    fn preallocate_closure_call_locals(
        &self,
        block: &TirBlock,
        type_table: &TypeTable,
        ctx: &mut FunctionContext,
    ) {
        // Count IndirectCall expressions and pre-allocate a unique temp local for each.
        // This supports nested closure calls without temp local collisions.
        let mut call_counts: HashMap<u32, u32> = HashMap::new();
        Self::count_indirect_calls_in_block(block, type_table, self, &mut call_counts);

        // Pre-allocate temp locals for each call site
        for (struct_type_idx, count) in call_counts {
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

    /// Count `IndirectCall` expressions in a block, grouped by closure struct type
    fn count_indirect_calls_in_block(
        block: &TirBlock,
        type_table: &TypeTable,
        codegen: &Codegen,
        counts: &mut HashMap<u32, u32>,
    ) {
        for stmt in &block.stmts {
            Self::count_indirect_calls_in_stmt(stmt, type_table, codegen, counts);
        }
    }

    fn count_indirect_calls_in_stmt(
        stmt: &TirStmt,
        type_table: &TypeTable,
        codegen: &Codegen,
        counts: &mut HashMap<u32, u32>,
    ) {
        match &stmt.kind {
            TirStmtKind::Let { value, .. } | TirStmtKind::Expr(value) => {
                Self::count_indirect_calls_in_expr(value, type_table, codegen, counts);
            }
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
                ..
            } => {
                Self::count_indirect_calls_in_expr(condition, type_table, codegen, counts);
                Self::count_indirect_calls_in_block(then_block, type_table, codegen, counts);
                if let Some(else_blk) = else_block {
                    Self::count_indirect_calls_in_block(else_blk, type_table, codegen, counts);
                }
            }
            TirStmtKind::While { condition, body } => {
                Self::count_indirect_calls_in_expr(condition, type_table, codegen, counts);
                Self::count_indirect_calls_in_block(body, type_table, codegen, counts);
            }
            TirStmtKind::For {
                condition,
                update,
                body,
                ..
            } => {
                if let Some(cond) = condition {
                    Self::count_indirect_calls_in_expr(cond, type_table, codegen, counts);
                }
                if let Some(upd) = update {
                    Self::count_indirect_calls_in_expr(upd, type_table, codegen, counts);
                }
                Self::count_indirect_calls_in_block(body, type_table, codegen, counts);
            }
            TirStmtKind::ForOf { iterable, body, .. } => {
                Self::count_indirect_calls_in_expr(iterable, type_table, codegen, counts);
                Self::count_indirect_calls_in_block(body, type_table, codegen, counts);
            }
            TirStmtKind::Loop { body } => {
                Self::count_indirect_calls_in_block(body, type_table, codegen, counts);
            }
            TirStmtKind::Return { value: Some(expr) } => {
                Self::count_indirect_calls_in_expr(expr, type_table, codegen, counts);
            }
            _ => {}
        }
    }

    fn count_indirect_calls_in_expr(
        expr: &TirExpr,
        type_table: &TypeTable,
        codegen: &Codegen,
        counts: &mut HashMap<u32, u32>,
    ) {
        match &expr.kind {
            TirExprKind::IndirectCall { callee, args } => {
                // Count this call
                let callee_type_id = callee.type_id;
                if let ResolvedType::Function {
                    params,
                    return_type,
                    ..
                } = type_table.get(callee_type_id)
                {
                    let key = (params.clone(), *return_type);
                    if let Some((_, _, struct_type_idx)) =
                        codegen.canonical_closure_types.borrow().get(&key).cloned()
                    {
                        *counts.entry(struct_type_idx).or_insert(0) += 1;
                    }
                }
                // Also count in callee and args
                Self::count_indirect_calls_in_expr(callee, type_table, codegen, counts);
                for arg in args {
                    Self::count_indirect_calls_in_expr(arg, type_table, codegen, counts);
                }
            }
            TirExprKind::Binary { left, right, .. } => {
                Self::count_indirect_calls_in_expr(left, type_table, codegen, counts);
                Self::count_indirect_calls_in_expr(right, type_table, codegen, counts);
            }
            TirExprKind::Unary { expr: operand, .. } => {
                Self::count_indirect_calls_in_expr(operand, type_table, codegen, counts);
            }
            TirExprKind::Call { args, .. } | TirExprKind::StaticCall { args, .. } => {
                for arg in args {
                    Self::count_indirect_calls_in_expr(arg, type_table, codegen, counts);
                }
            }
            TirExprKind::MethodCall { receiver, args, .. } => {
                Self::count_indirect_calls_in_expr(receiver, type_table, codegen, counts);
                for arg in args {
                    Self::count_indirect_calls_in_expr(arg, type_table, codegen, counts);
                }
            }
            TirExprKind::StructLiteral { fields, .. } => {
                for field in fields {
                    Self::count_indirect_calls_in_expr(&field.value, type_table, codegen, counts);
                }
            }
            TirExprKind::FieldAccess { expr, .. } => {
                Self::count_indirect_calls_in_expr(expr, type_table, codegen, counts);
            }
            TirExprKind::Index { expr, index, .. } => {
                Self::count_indirect_calls_in_expr(expr, type_table, codegen, counts);
                Self::count_indirect_calls_in_expr(index, type_table, codegen, counts);
            }
            TirExprKind::Block(block) => {
                Self::count_indirect_calls_in_block(block, type_table, codegen, counts);
            }
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                Self::count_indirect_calls_in_expr(condition, type_table, codegen, counts);
                Self::count_indirect_calls_in_block(then_branch, type_table, codegen, counts);
                if let Some(else_blk) = else_branch {
                    Self::count_indirect_calls_in_block(else_blk, type_table, codegen, counts);
                }
            }
            TirExprKind::ArrayLiteral { elements } | TirExprKind::TupleLiteral { elements } => {
                for elem in elements {
                    Self::count_indirect_calls_in_expr(elem, type_table, codegen, counts);
                }
            }
            TirExprKind::Closure { body, .. } => {
                Self::count_indirect_calls_in_expr(body, type_table, codegen, counts);
            }
            TirExprKind::Cast { expr: inner, .. } => {
                Self::count_indirect_calls_in_expr(inner, type_table, codegen, counts);
            }
            TirExprKind::Assign { target, value } => {
                Self::count_indirect_calls_in_expr(target, type_table, codegen, counts);
                Self::count_indirect_calls_in_expr(value, type_table, codegen, counts);
            }
            TirExprKind::Move { value } => {
                Self::count_indirect_calls_in_expr(value, type_table, codegen, counts);
            }
            _ => {}
        }
    }

    /// Pre-allocate locals for `Array#append()` method calls.
    /// This scans the TIR for append calls and pre-allocates the 5 locals needed per element type.
    fn preallocate_array_append_locals(
        &self,
        block: &TirBlock,
        type_table: &TypeTable,
        ctx: &mut FunctionContext,
    ) {
        // Collect unique array element types that have append calls
        let mut append_element_types: HashSet<TypeId> = HashSet::new();
        Self::collect_array_append_types(block, type_table, &mut append_element_types);

        // Pre-allocate locals for each element type
        for element_type in append_element_types {
            if let Some(&array_struct_type_idx) = self.array_struct_types.get(&element_type)
                && let Some(&raw_array_type_idx) = self.array_types.get(&element_type)
            {
                let element_valtype = self.type_id_to_valtype(type_table, element_type);
                let array_struct_valtype = ValType::Ref(RefType {
                    nullable: true,
                    heap_type: HeapType::Concrete(array_struct_type_idx),
                });
                let raw_array_valtype = ValType::Ref(RefType {
                    nullable: true,
                    heap_type: HeapType::Concrete(raw_array_type_idx),
                });

                // Pre-allocate the 5 locals needed for append
                ctx.alloc_local("__append_array", array_struct_valtype);
                ctx.alloc_local("__append_value", element_valtype);
                ctx.alloc_local("__append_used", ValType::I32);
                ctx.alloc_local("__append_capacity", ValType::I32);
                ctx.alloc_local("__append_new_repr", raw_array_valtype);
            }
        }
    }

    /// Collect element types of arrays that have `append()` called on them
    fn collect_array_append_types(
        block: &TirBlock,
        type_table: &TypeTable,
        result: &mut HashSet<TypeId>,
    ) {
        for stmt in &block.stmts {
            Self::collect_array_append_types_from_stmt(stmt, type_table, result);
        }
    }

    fn collect_array_append_types_from_stmt(
        stmt: &TirStmt,
        type_table: &TypeTable,
        result: &mut HashSet<TypeId>,
    ) {
        match &stmt.kind {
            TirStmtKind::Let { value, .. } | TirStmtKind::Expr(value) => {
                Self::collect_array_append_types_from_expr(value, type_table, result);
            }
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
                ..
            } => {
                Self::collect_array_append_types_from_expr(condition, type_table, result);
                Self::collect_array_append_types(then_block, type_table, result);
                if let Some(else_blk) = else_block {
                    Self::collect_array_append_types(else_blk, type_table, result);
                }
            }
            TirStmtKind::While { condition, body } => {
                Self::collect_array_append_types_from_expr(condition, type_table, result);
                Self::collect_array_append_types(body, type_table, result);
            }
            TirStmtKind::For {
                condition,
                update,
                body,
            } => {
                if let Some(cond) = condition {
                    Self::collect_array_append_types_from_expr(cond, type_table, result);
                }
                if let Some(upd) = update {
                    Self::collect_array_append_types_from_expr(upd, type_table, result);
                }
                Self::collect_array_append_types(body, type_table, result);
            }
            TirStmtKind::ForOf { iterable, body, .. } => {
                Self::collect_array_append_types_from_expr(iterable, type_table, result);
                Self::collect_array_append_types(body, type_table, result);
            }
            TirStmtKind::Loop { body } => {
                Self::collect_array_append_types(body, type_table, result);
            }
            TirStmtKind::Return { value: Some(expr) } => {
                Self::collect_array_append_types_from_expr(expr, type_table, result);
            }
            _ => {}
        }
    }

    fn collect_array_append_types_from_expr(
        expr: &TirExpr,
        type_table: &TypeTable,
        result: &mut HashSet<TypeId>,
    ) {
        match &expr.kind {
            TirExprKind::MethodCall {
                receiver,
                func: method_func,
                args,
                ..
            } => {
                // Extract method name from func reference
                let method_name = {
                    let name = method_func.name();
                    if let Some(pos) = name.rfind("::") {
                        name[pos + 2..].to_string()
                    } else {
                        name
                    }
                };
                // Check if this is an append call on an Array type
                if method_name == "append"
                    && let Some(element_type) = type_table.as_array(receiver.type_id)
                {
                    result.insert(element_type);
                }
                Self::collect_array_append_types_from_expr(receiver, type_table, result);
                for arg in args {
                    Self::collect_array_append_types_from_expr(arg, type_table, result);
                }
            }
            TirExprKind::Binary { left, right, .. } => {
                Self::collect_array_append_types_from_expr(left, type_table, result);
                Self::collect_array_append_types_from_expr(right, type_table, result);
            }
            TirExprKind::Unary { expr: inner, .. }
            | TirExprKind::Cast { expr: inner, .. }
            | TirExprKind::FieldAccess { expr: inner, .. } => {
                Self::collect_array_append_types_from_expr(inner, type_table, result);
            }
            TirExprKind::Call { args, .. }
            | TirExprKind::EffectCall { args, .. }
            | TirExprKind::StaticCall { args, .. } => {
                for arg in args {
                    Self::collect_array_append_types_from_expr(arg, type_table, result);
                }
            }
            TirExprKind::Index { expr, index } => {
                Self::collect_array_append_types_from_expr(expr, type_table, result);
                Self::collect_array_append_types_from_expr(index, type_table, result);
            }
            TirExprKind::Assign { target, value } => {
                Self::collect_array_append_types_from_expr(target, type_table, result);
                Self::collect_array_append_types_from_expr(value, type_table, result);
            }
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                Self::collect_array_append_types_from_expr(condition, type_table, result);
                Self::collect_array_append_types(then_branch, type_table, result);
                if let Some(else_blk) = else_branch {
                    Self::collect_array_append_types(else_blk, type_table, result);
                }
            }
            TirExprKind::Block(block) => {
                Self::collect_array_append_types(block, type_table, result);
            }
            TirExprKind::ArrayLiteral { elements } | TirExprKind::TupleLiteral { elements } => {
                for elem in elements {
                    Self::collect_array_append_types_from_expr(elem, type_table, result);
                }
            }
            TirExprKind::StructLiteral { fields, .. } => {
                for field in fields {
                    Self::collect_array_append_types_from_expr(&field.value, type_table, result);
                }
            }
            TirExprKind::Closure { body, .. } => {
                Self::collect_array_append_types_from_expr(body, type_table, result);
            }
            TirExprKind::IndirectCall { callee, args } => {
                Self::collect_array_append_types_from_expr(callee, type_table, result);
                for arg in args {
                    Self::collect_array_append_types_from_expr(arg, type_table, result);
                }
            }
            _ => {}
        }
    }

    /// Pre-allocate locals from Let statements in a block (used for closure block bodies)
    fn preallocate_locals_from_block(
        &self,
        block: &TirBlock,
        type_table: &TypeTable,
        ctx: &mut FunctionContext,
    ) {
        for stmt in &block.stmts {
            self.preallocate_locals_from_stmt(stmt, type_table, ctx);
        }
    }

    fn preallocate_locals_from_stmt(
        &self,
        stmt: &TirStmt,
        type_table: &TypeTable,
        ctx: &mut FunctionContext,
    ) {
        match &stmt.kind {
            TirStmtKind::Let {
                local_index,
                type_id,
                ..
            } => {
                let local_type = self.type_id_to_valtype(type_table, *type_id);
                let local_name = format!("_local_{local_index}");
                ctx.alloc_local(&local_name, local_type);
            }
            TirStmtKind::If {
                then_block,
                else_block,
                ..
            } => {
                self.preallocate_locals_from_block(then_block, type_table, ctx);
                if let Some(else_blk) = else_block {
                    self.preallocate_locals_from_block(else_blk, type_table, ctx);
                }
            }
            TirStmtKind::While { body, .. }
            | TirStmtKind::For { body, .. }
            | TirStmtKind::ForOf { body, .. }
            | TirStmtKind::Loop { body } => {
                self.preallocate_locals_from_block(body, type_table, ctx);
            }
            _ => {}
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
            .map(|(_, ty)| wasi_type_to_valtype(ty))
            .collect();

        // Async functions have an additional outptr parameter for the result
        if func.is_async {
            params.push(ValType::I32); // outptr
        }
        // Sync functions with complex return types also need an outptr
        else if let Some(ret_ty) = &func.return_type
            && return_type_requires_outptr(ret_ty)
        {
            params.push(ValType::I32); // outptr
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
            // Complex types are returned via outptr, so no direct return value
            if return_type_requires_outptr(ret_ty) {
                vec![]
            } else {
                vec![wasi_type_to_valtype(ret_ty)]
            }
        } else {
            vec![]
        }
    }

    /// Convert a builtin function type to Core Wasm params
    fn builtin_func_to_core_params(&self, func: &BuiltinFunctionInfo) -> Vec<ValType> {
        func.params
            .iter()
            .map(|(_, ty)| wasi_type_to_valtype(ty))
            .collect()
    }

    /// Convert a builtin function type to Core Wasm results
    fn builtin_func_to_core_results(&self, func: &BuiltinFunctionInfo) -> Vec<ValType> {
        if func.diverges {
            // Diverging functions have no return type
            vec![]
        } else if let Some(ret_ty) = &func.return_type {
            vec![wasi_type_to_valtype(ret_ty)]
        } else {
            vec![]
        }
    }

    /// Convert a world export function type to Core Wasm params
    ///
    /// For async exports, the core function has no params (async uses `task_return`).
    /// For sync exports, params are mapped directly.
    fn world_export_to_core_params(&self, export: &WorldExportInfo) -> Vec<ValType> {
        if export.is_async {
            // Async exports have no params in core (lifted signature differs)
            vec![]
        } else {
            export
                .params
                .iter()
                .map(|(_, ty)| wasi_type_to_valtype(ty))
                .collect()
        }
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
            vec![wasi_type_to_valtype(ret_ty)]
        } else {
            vec![]
        }
    }

    /// Build the memory module (provides shared memory and realloc for all core modules)
    fn build_memory_module(&self, string_data: &[u8], strip_names: bool) -> Vec<u8> {
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

        // Data section: string literals
        if !string_data.is_empty() {
            let mut data = DataSection::new();
            data.segment(DataSegment {
                mode: DataSegmentMode::Active {
                    memory_index: 0,
                    offset: &ConstExpr::i32_const(0),
                },
                data: string_data.iter().copied(),
            });
            module.section(&data);
        }

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
            crate::OptLevel::default(),
        )
        .await
        .expect("compilation failed");

        // Verify it starts with Wasm magic number
        let wasm = result.wasm;
        assert!(wasm.len() > 8);
        assert_eq!(&wasm[0..4], b"\0asm");
    }
}
