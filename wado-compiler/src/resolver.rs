//! Type resolution phase for Wado
//!
//! The type resolver:
//! 1. Takes the desugared AST and symbol table from the analyzer
//! 2. Performs type inference and type checking
//! 3. Produces the Typed Intermediate Representation (TIR)
//!
//! All type resolution happens in this phase. The output TIR has fully
//! resolved types on every expression, making code generation mechanical.

mod call;
mod closure;
mod coercion;
mod expr;
mod infer;
mod item;
mod method_call;
mod method_lookup;
mod module;
mod operators;
pub(crate) mod orchestration;
mod stmt;
mod template;
mod trait_env;
mod trait_query;
mod type_resolution;
mod typecheck;
pub(crate) mod types;
mod util;

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use crate::hashmap::{IndexMap, IndexSet};

use crate::ast::{self, Item, Module};
use crate::builtin_registry::BuiltinRegistry;
use crate::compiler_host::CompilerHost;
use crate::component_model::WasiRegistry;
use crate::logger::{Bail, Logger};
use crate::name::{self as name, MethodName, ModuleSource};
use crate::symbol::SymbolTable;
use crate::tir::{
    self as tir, TirEnum, TirEnumCase, TirFlags, TirFlagsMember, TirModule, TirNewtype, TypeId,
    TypeTable,
};

use trait_env::TraitEnv;
pub use types::TypeError;
use types::{
    EnumInfo, FlagsInfo, GenericNewtypeInfo, ModuleTypeMaps, ResourceInfo, StructFieldInfo,
    VariantInfo,
};

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
    /// Generic newtype definitions (name -> info) - flat map for current module
    generic_newtype_defs: IndexMap<String, GenericNewtypeInfo>,
    /// Struct field info (struct name -> (`module_source`, fields)) - flat map for current module
    struct_fields: IndexMap<String, StructFieldInfo>,
    /// Variant case info (variant name -> (`module_source`, `type_params`, cases)) - flat map for current module
    variant_cases: IndexMap<String, VariantInfo>,
    /// Enum case info (enum name -> (`module_source`, cases)) - flat map for current module
    enum_cases: IndexMap<String, EnumInfo>,
    /// Flags type info (flags name -> (`module_source`, `type_id`, members)) - flat map for current module
    flags_cases: IndexMap<String, FlagsInfo>,
    /// Resource info (resource name -> module source and methods) - flat map for current module
    resource_types: IndexMap<String, ResourceInfo>,
    /// Per-module nested maps for cross-module type resolution (shared via Rc)
    all_newtypes: Rc<IndexMap<ModuleSource, IndexMap<String, TypeId>>>,
    all_struct_fields: Rc<IndexMap<ModuleSource, IndexMap<String, StructFieldInfo>>>,
    all_variant_cases: Rc<IndexMap<ModuleSource, IndexMap<String, VariantInfo>>>,
    all_enum_cases: Rc<IndexMap<ModuleSource, IndexMap<String, EnumInfo>>>,
    all_flags_cases: Rc<IndexMap<ModuleSource, IndexMap<String, FlagsInfo>>>,
    all_resource_types: Rc<IndexMap<ModuleSource, IndexMap<String, ResourceInfo>>>,
    /// Function return types (name -> return type)
    function_return_types: IndexMap<String, TypeId>,
    /// Imported function names for the current module
    imported_functions: IndexSet<String>,
    /// Logger for emitting diagnostics
    logger: &'a Logger<'a, H>,
    /// Current module source being resolved (for struct type `module_source`)
    current_module_source: ModuleSource,
    /// Entry module source (for cross-module import dedup)
    entry_module_source: ModuleSource,
    /// Current module items (for local function parameter lookup)
    current_module_items: &'a [Item],
    /// Mutable trait resolution context: type params, bounds, associated type bindings, self type.
    /// Grouped together so scope entry/exit can save/restore the whole context at once.
    trait_ctx: trait_env::TraitContext,
    /// Generic struct definitions (name -> type param count)
    /// Used to determine if a struct is generic
    generic_struct_names: IndexSet<String>,
    /// Generic function type parameters (`func_name` -> `type_params`)
    /// Used for substituting type parameters in return types
    generic_function_params: IndexMap<String, Vec<(String, TypeId)>>,
    /// Resolved param types for generic functions (`func_name` -> `param TypeIds`)
    /// Resolved in the function's own type param scope so `TypeParams` have correct ids.
    generic_function_resolved_param_types: IndexMap<String, Vec<TypeId>>,
    /// Resolved return type for generic functions (`func_name` -> `return TypeId`)
    /// Resolved in the function's own type param scope; used for expected-return
    /// driven back-inference by [`infer::InferCtx::add_expected_return`].
    generic_function_resolved_return_types: IndexMap<String, TypeId>,
    /// Generic method type parameters (`mangled_name` -> `type_params`)
    /// Used for substituting type parameters in method return types
    generic_method_params: IndexMap<String, Vec<(String, TypeId)>>,
    /// Resolved param types for generic methods (`mangled_name` -> `param TypeIds`)
    /// Resolved in the method's own type param scope so `TypeParams` have correct ids.
    generic_method_resolved_param_types: IndexMap<String, Vec<TypeId>>,
    /// Namespace import aliases (e.g., "helper" -> module source for `use helper from "..."`)
    namespace_imports: IndexMap<String, ModuleSource>,
    /// Effect name to source module mapping (e.g., "Stdout" -> wasi:cli module source)
    /// Built from import declarations and local effect declarations.
    effect_sources: IndexMap<String, ModuleSource>,
    /// Effect parameter names currently in scope (from enclosing function's `<effect E>`)
    current_effect_params: IndexSet<String>,
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
    /// Immutable trait knowledge base: impl indices, trait declarations, and blanket impls.
    /// Built once and shared across all module resolvers via `Arc`.
    trait_env: Arc<TraitEnv>,
    /// Pre-loaded file contents for `#include_str` / `#include_bytes`.
    /// Key: `[module_source_display, raw_path]`, value: raw bytes.
    included_files: &'a IndexMap<[String; 2], Vec<u8>>,
    /// Cached flat set of all known type names for fast `is_known_type_name` lookups.
    known_type_names_cache: IndexSet<String>,
    /// Anonymous structs created during expression resolution.
    /// Flushed into the `TirModule` at the end of `resolve_module`.
    pending_anonymous_structs: Vec<crate::tir::TirStruct>,
    /// Cache for `find_indexing_trait_impl` results.
    /// Key: (`struct_name`, `base_type_id`, `trait_base_name`, `method_name`, `assoc_type_name`)
    indexing_trait_cache: IndexMap<
        (String, TypeId, String, String, String),
        Option<(TypeId, ast::SelfKind, String, ModuleSource)>,
    >,
    /// Recursion guard for `type_implements_trait` to avoid infinite recursion
    /// on recursive types (e.g., variant Elem containing struct `RepeatElem` with field Elem).
    trait_check_stack: RefCell<Vec<(TypeId, String)>>,
    /// Cache for `lookup_method_info` results.
    /// Key: (`base_type_id`, `method_name`) → cached `MethodInfo`
    method_info_cache: IndexMap<(TypeId, String), Option<types::MethodInfo>>,
    /// Index from function name → position in `current_module_items` for O(1) lookup.
    current_module_func_index: IndexMap<String, usize>,
    /// Per-module index from function name → position in module.items for O(1) lookup.
    loaded_module_func_indices: IndexMap<ModuleSource, IndexMap<String, usize>>,
}

impl<'a, H: CompilerHost> Resolver<'a, H> {
    pub fn new(
        symbols: &'a SymbolTable,
        loaded_modules: &'a IndexMap<ModuleSource, Module>,
        builtin_registry: &'a BuiltinRegistry,
        logger: &'a Logger<'a, H>,
        included_files: &'a IndexMap<[String; 2], Vec<u8>>,
    ) -> Self {
        let (wasi_registry, _) = WasiRegistry::build_from_stdlib();
        let type_table = Rc::new(RefCell::new(TypeTable::new()));
        let (trait_env, _) = TraitEnv::build(loaded_modules);
        Self {
            type_table,
            symbols,
            loaded_modules,
            newtypes: IndexMap::default(),
            generic_newtype_defs: IndexMap::default(),
            struct_fields: IndexMap::default(),
            variant_cases: IndexMap::default(),
            enum_cases: IndexMap::default(),
            flags_cases: IndexMap::default(),
            resource_types: IndexMap::default(),
            all_newtypes: Rc::new(IndexMap::default()),
            all_struct_fields: Rc::new(IndexMap::default()),
            all_variant_cases: Rc::new(IndexMap::default()),
            all_enum_cases: Rc::new(IndexMap::default()),
            all_flags_cases: Rc::new(IndexMap::default()),
            all_resource_types: Rc::new(IndexMap::default()),
            function_return_types: IndexMap::default(),
            imported_functions: IndexSet::default(),
            namespace_imports: IndexMap::default(),
            logger,
            current_module_source: ModuleSource::entry_point_with_filename("<uninitialized>"),
            entry_module_source: ModuleSource::entry_point_with_filename("<uninitialized>"),
            current_module_items: &[],
            effect_sources: IndexMap::default(),
            current_effect_params: IndexSet::default(),
            trait_ctx: trait_env::TraitContext::default(),
            generic_struct_names: IndexSet::default(),
            generic_function_params: IndexMap::default(),
            generic_function_resolved_param_types: IndexMap::default(),
            generic_function_resolved_return_types: IndexMap::default(),
            generic_method_params: IndexMap::default(),
            generic_method_resolved_param_types: IndexMap::default(),
            wasi_registry,
            builtin_registry,
            current_module_globals: IndexMap::default(),
            imported_globals: IndexMap::default(),
            associated_constants: IndexMap::default(),
            module_type_maps_cache: IndexMap::default(),
            trait_env,
            included_files,
            known_type_names_cache: IndexSet::default(),
            indexing_trait_cache: IndexMap::default(),
            trait_check_stack: RefCell::new(Vec::new()),
            method_info_cache: IndexMap::default(),
            pending_anonymous_structs: Vec::new(),
            current_module_func_index: IndexMap::default(),
            loaded_module_func_indices: IndexMap::default(),
        }
    }

    /// Build a function-name → index map for a module's items.
    fn build_func_index(items: &[Item]) -> IndexMap<String, usize> {
        let mut index = IndexMap::default();
        for (i, item) in items.iter().enumerate() {
            if let Item::Function(func) = item {
                index.insert(func.name.clone(), i);
            }
        }
        index
    }

    /// Look up a function by name in a loaded module, returning the Item at that index.
    fn lookup_func_in_loaded_module<'b>(
        loaded_modules: &'b IndexMap<ModuleSource, Module>,
        loaded_module_func_indices: &IndexMap<ModuleSource, IndexMap<String, usize>>,
        module_source: &ModuleSource,
        func_name: &str,
    ) -> Option<&'b ast::Function> {
        let idx_map = loaded_module_func_indices.get(module_source)?;
        let &idx = idx_map.get(func_name)?;
        let module = loaded_modules.get(module_source)?;
        if let Item::Function(func) = &module.items[idx] {
            Some(func)
        } else {
            None
        }
    }

    /// Look up a function by name in current module items.
    fn lookup_current_func(&self, func_name: &str) -> Option<&ast::Function> {
        let &idx = self.current_module_func_index.get(func_name)?;
        if let Item::Function(func) = &self.current_module_items[idx] {
            Some(func)
        } else {
            None
        }
    }

    /// Look up struct field info by (name, `module_source`).
    ///
    /// This is the correct way to look up struct fields — it disambiguates
    /// The flat `struct_fields` map is a visibility-scoped projection of `all_struct_fields`,
    /// containing only types visible to the current module (own definitions + explicit imports).
    ///
    /// `all_struct_fields` covers ALL modules and is needed for cross-module lookups where
    /// the type wasn't explicitly imported (e.g., a struct returned by a function from another
    /// module).
    fn lookup_struct_fields(
        &self,
        name: &str,
        module_source: &ModuleSource,
    ) -> Option<&StructFieldInfo> {
        self.struct_fields
            .get(name)
            .filter(|info| info.module_source == *module_source)
            .or_else(|| {
                self.all_struct_fields
                    .get(module_source)
                    .and_then(|m| m.get(name))
            })
    }

    /// Build effect name → module source map from a module's import declarations.
    ///
    /// For `use { Stdout::{write_via_stream} } from "wasi:cli"`, maps "Stdout" → resolved("wasi:cli").
    /// For `use { Stdout } from "core:cli"`, maps "Stdout" → resolved("core:cli").
    /// For local effect declarations, maps name → current module source.
    fn build_effect_sources(
        module: &Module,
        module_source: &ModuleSource,
    ) -> IndexMap<String, ModuleSource> {
        let mut sources = IndexMap::default();
        for item in &module.items {
            match item {
                Item::Use(use_decl) => {
                    let source = name::resolve_import(module_source, &use_decl.source);
                    for use_item in &use_decl.items {
                        match use_item {
                            ast::UseItem::EffectFunctions { effect_name, .. } => {
                                sources.insert(effect_name.clone(), source.clone());
                            }
                            ast::UseItem::Simple { name, alias } => {
                                // Track simple imports that look like effect names (PascalCase)
                                let local_name = alias.as_ref().unwrap_or(name);
                                if local_name.starts_with(|c: char| c.is_ascii_uppercase()) {
                                    sources.insert(local_name.clone(), source.clone());
                                }
                            }
                            ast::UseItem::Wildcard | ast::UseItem::Namespace { .. } => {}
                        }
                    }
                }
                Item::Effect(effect_decl) => {
                    sources.insert(effect_decl.name.clone(), module_source.clone());
                }
                _ => {}
            }
        }
        sources
    }

    /// Resolve AST effect names (strings) to TIR `EffectRefs` with module source information.
    pub(crate) fn resolve_effects(&self, effects: &[String]) -> Vec<tir::EffectRef> {
        effects
            .iter()
            .map(|name| {
                if self.current_effect_params.contains(name) {
                    tir::EffectRef::Param { name: name.clone() }
                } else if let Some(source) = self.effect_sources.get(name) {
                    tir::EffectRef::Concrete {
                        name: name.clone(),
                        module_source: source.clone(),
                    }
                } else {
                    // Fallback: effect from current module (local effect declaration)
                    tir::EffectRef::Concrete {
                        name: name.clone(),
                        module_source: self.current_module_source.clone(),
                    }
                }
            })
            .collect()
    }

    /// Resolve a module, converting AST to TIR
    pub fn resolve_module(
        &mut self,
        module: &'a Module,
        module_source: ModuleSource,
    ) -> Result<TirModule, Bail> {
        // Set current module source for struct type creation
        self.current_module_source = module_source.clone();
        // Store current module items as a reference (no clone)
        self.current_module_items = &module.items;
        // Build function name → index for O(1) lookup
        self.current_module_func_index = Self::build_func_index(self.current_module_items);
        // Clear trait lookup caches (current_module_items changed)
        self.indexing_trait_cache.clear();
        // Build effect source map from imports
        self.effect_sources = Self::build_effect_sources(module, &module_source);

        // First pass: collect type definitions
        {
            let _span = self.logger.span("resolve/collect_types");
            self.collect_types(module);
        }

        // Second pass: collect function signatures (for call resolution)
        {
            let _span = self.logger.span("resolve/collect_sigs");
            self.collect_function_signatures(module);
        }

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
        let _resolve_funcs_span = self.logger.span("resolve/resolve_funcs");
        let mut tir_module = TirModule::new(module_source);

        // Pre-populate the generic-function inference caches for every
        // generic function in the current module. This allows same-module
        // forward references (e.g. `outer<T>` calling `inner<T>` defined
        // later in the file) to infer type arguments at the call site
        // during body resolution, without relying on a later
        // monomorphization-time fallback.
        for item in &module.items {
            if let Item::Function(func) = item {
                self.precompute_generic_function_cache(func);
            }
        }

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
                        .map(|t| self.get_type_name_full(t));

                    // Register type parameters from impl block's generic type FIRST
                    // e.g., impl IndexValue<i32> for Triple<T> needs T registered
                    let saved_trait_ctx = self.trait_ctx.clone();
                    self.trait_ctx.type_params.clear();
                    self.trait_ctx.type_param_bounds.clear();

                    // Register explicit type params from impl<T: Bound> declarations,
                    // skipping concrete types (e.g., `impl<i32, T>` — skip "i32").
                    // This handles both `impl<T> Trait for Struct<T>` and
                    // `impl<T: Bound> OtherTrait for T` (T is the impl type directly).
                    let mut actual_idx = 0u32;
                    for param in &impl_block.type_params {
                        if self.is_known_type_name(&param.name) {
                            // Concrete type in explicit params (e.g., `impl<i32, T>`): skip
                            if !param.bounds.is_empty() {
                                self.trait_ctx
                                    .type_param_bounds
                                    .entry(param.name.clone())
                                    .or_default()
                                    .extend(param.bounds.clone());
                            }
                            continue;
                        }
                        if !self.trait_ctx.type_params.contains_key(&param.name) {
                            let type_id = if param.is_pack {
                                self.type_table
                                    .borrow_mut()
                                    .make_type_pack(param.name.clone(), actual_idx)
                            } else {
                                self.type_table
                                    .borrow_mut()
                                    .make_type_param(param.name.clone(), actual_idx)
                            };
                            self.trait_ctx
                                .type_params
                                .insert(param.name.clone(), (actual_idx, type_id));
                        }
                        if !param.bounds.is_empty() {
                            self.trait_ctx
                                .type_param_bounds
                                .entry(param.name.clone())
                                .or_default()
                                .extend(param.bounds.clone());
                        }
                        actual_idx += 1;
                    }

                    // Unwrap reference for ref-type impls (impl Trait for &Container<T>)
                    let impl_inner_ty = match &impl_block.ty {
                        ast::Type::Reference(inner) | ast::Type::MutReference(inner) => {
                            inner.as_ref()
                        }
                        other => other,
                    };
                    if let ast::Type::Generic(generic) = impl_inner_ty {
                        for (i, arg) in generic.args.iter().enumerate() {
                            if let ast::Type::Named(named) = arg {
                                let name = &named.name;
                                if !self.trait_ctx.type_params.contains_key(name)
                                    && !self.is_known_type_name(name)
                                {
                                    let type_id = self
                                        .type_table
                                        .borrow_mut()
                                        .make_type_param(name.clone(), i as u32);
                                    self.trait_ctx
                                        .type_params
                                        .insert(name.clone(), (i as u32, type_id));
                                }
                            }
                        }
                    }

                    // `impl Trait for Type;` — record synthesis request and skip
                    if impl_block.is_synthesize_request {
                        if let Some(ref trait_type) = impl_block.trait_type {
                            let synth_trait_name = self.get_type_name_full(trait_type);
                            let target_type_id = self.resolve_type(&impl_block.ty);
                            let type_params: Vec<_> = self
                                .trait_ctx
                                .type_params
                                .iter()
                                .map(|(name, &(index, type_id))| (name.clone(), index, type_id))
                                .collect();
                            tir_module
                                .synthesis_requests
                                .push(crate::tir::SynthesisRequest {
                                    trait_name: synth_trait_name,
                                    target_type_name: struct_name.clone(),
                                    target_type_id,
                                    type_params,
                                    span: impl_block.span,
                                });
                        }
                        self.trait_ctx = saved_trait_ctx;
                        continue;
                    }

                    // Set up associated type bindings for trait implementations
                    // This now works because type params (like T) are registered above
                    self.trait_ctx.assoc_type_bindings.clear();
                    if impl_block.trait_type.is_some() {
                        // Resolve the target type for registering associated type resolutions
                        let target_type_id = self.resolve_type(&impl_block.ty);
                        let is_concrete =
                            !self.type_table.borrow().contains_type_param(target_type_id);

                        for binding in &impl_block.associated_types {
                            let type_id = self.resolve_type(&binding.ty);
                            self.trait_ctx
                                .assoc_type_bindings
                                .insert(binding.name.clone(), type_id);

                            // Register in TypeTable for substitution resolution
                            // Only for concrete types (not generic impls like impl<T> Trait for Array<T>)
                            if is_concrete {
                                self.type_table.borrow_mut().register_assoc_type_resolution(
                                    target_type_id,
                                    binding.name.clone(),
                                    type_id,
                                );
                            } else {
                                // For generic impls, register the definition so the monomorphizer
                                // can resolve associated types for GenericInstance types.
                                self.type_table
                                    .borrow_mut()
                                    .register_generic_assoc_type_def(
                                        struct_name.clone(),
                                        binding.name.clone(),
                                        type_id,
                                    );
                            }
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

                    // Restore trait context
                    self.trait_ctx = saved_trait_ctx;
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
                    let module_is_todo = module.has_todo();
                    if let Some((tir_func, tir_test)) =
                        self.resolve_test_decl(test_decl, test_index, module_is_todo)
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
                        module_source: self.current_module_source.clone(),
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
                Item::Flags(flags_decl) => {
                    if let Some(flags_info) = self.flags_cases.get(&flags_decl.name) {
                        let tir_flags = TirFlags {
                            name: flags_decl.name.clone(),
                            module_source: self.current_module_source.clone(),
                            is_pub: flags_decl.is_pub,
                            type_id: flags_info.type_id,
                            members: flags_decl
                                .flags
                                .iter()
                                .enumerate()
                                .map(|(i, m)| TirFlagsMember {
                                    name: m.name.clone(),
                                    bitmask: 1u32 << i,
                                    span: m.span,
                                })
                                .collect(),
                            span: flags_decl.span,
                        };
                        tir_module.add_flags(tir_flags);
                    }
                }
                Item::Newtype(newtype_decl) => {
                    if let Some(&type_id) = self.newtypes.get(&newtype_decl.name) {
                        tir_module.add_newtype(TirNewtype {
                            name: newtype_decl.name.clone(),
                            module_source: self.current_module_source.clone(),
                            is_pub: newtype_decl.is_pub,
                            type_id,
                            span: newtype_decl.span,
                        });
                    }
                }
                // Other items will be added as needed
                _ => {}
            }
        }

        drop(_resolve_funcs_span);
        // Share the type table via Rc::clone
        tir_module.type_table = Rc::clone(&self.type_table);

        // Preserve data section
        if let Some(data) = module.data_section() {
            tir_module = tir_module.with_data_section(Some(data.to_string()));
        }

        // Add anonymous structs created during expression resolution
        for anon_struct in self.pending_anonymous_structs.drain(..) {
            tir_module.add_struct(anon_struct);
        }

        // Preserve wasm_module attribute
        tir_module.wasm_module = module.wasm_module().map(String::from);

        self.logger.ok_or_bail(tir_module)
    }

    /// Get the type table (after resolution)
    pub fn into_type_table(self) -> Rc<RefCell<TypeTable>> {
        self.type_table
    }
}

pub fn resolve_module<H: CompilerHost>(
    module: &Module,
    module_source: ModuleSource,
    symbols: &SymbolTable,
    loaded_modules: &IndexMap<ModuleSource, Module>,
    logger: &Logger<H>,
) -> Result<TirModule, Bail> {
    let type_table = std::cell::RefCell::new(crate::tir::TypeTable::new());
    let builtin_registry = BuiltinRegistry::build_from_stdlib(&type_table);
    let empty_included = IndexMap::default();
    let mut resolver = Resolver::new(
        symbols,
        loaded_modules,
        &builtin_registry,
        logger,
        &empty_included,
    );
    resolver.resolve_module(module, module_source)
}
