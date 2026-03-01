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
mod item;
mod method_call;
mod method_lookup;
mod module;
mod operators;
mod orchestration;
mod stmt;
mod template;
mod type_resolution;
pub(crate) mod types;
mod util;

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use indexmap::{IndexMap, IndexSet};

use crate::ast::{self, Item, Module};
use crate::builtin_registry::BuiltinRegistry;
use crate::compiler_host::CompilerHost;
use crate::component_model::WasiRegistry;
use crate::logger::{Bail, Logger};
use crate::name::{self as name, MethodName, ModuleSource};
use crate::project::Project;
use crate::symbol::SymbolTable;
use crate::tir::{
    TirEnum, TirEnumCase, TirFlags, TirFlagsMember, TirModule, TirNewtype, TypeId, TypeTable,
};

pub use types::TypeError;
use types::{
    BlanketTraitImplIndex, EnumInfo, FlagsInfo, ModuleTypeMaps, ResourceInfo, StructFieldInfo,
    TraitDeclIndex, TraitImplIndex, VariantInfo,
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
    /// Per-module nested maps for cross-module type resolution
    all_newtypes: IndexMap<ModuleSource, IndexMap<String, TypeId>>,
    all_struct_fields: IndexMap<ModuleSource, IndexMap<String, StructFieldInfo>>,
    all_variant_cases: IndexMap<ModuleSource, IndexMap<String, VariantInfo>>,
    all_enum_cases: IndexMap<ModuleSource, IndexMap<String, EnumInfo>>,
    all_flags_cases: IndexMap<ModuleSource, IndexMap<String, FlagsInfo>>,
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
    /// Pre-built index: type name → (`module_source`, `item_idx`) for trait impl blocks in `loaded_modules`.
    /// Shared across all module Resolvers; avoids O(all items) scans in `find_trait_method_for_type`.
    trait_impl_index: Arc<TraitImplIndex>,
    /// Pre-built index: trait name → (`module_source`, `item_idx`) for trait declarations in `loaded_modules`.
    trait_decl_index: Arc<TraitDeclIndex>,
    /// Pre-built list of blanket trait impl blocks: `impl<T: Trait> OtherTrait for T`.
    /// Checked as fallback when concrete type lookup in `trait_impl_index` fails.
    blanket_trait_impl_index: Arc<BlanketTraitImplIndex>,
}

impl<'a, H: CompilerHost> Resolver<'a, H> {
    pub fn new(
        symbols: &'a SymbolTable,
        loaded_modules: &'a IndexMap<ModuleSource, Module>,
        builtin_registry: &'a BuiltinRegistry,
        logger: &'a Logger<'a, H>,
    ) -> Self {
        let (wasi_registry, _) = WasiRegistry::build_from_stdlib();
        let type_table = Rc::new(RefCell::new(TypeTable::new()));
        let (trait_impl_index, trait_decl_index, blanket_trait_impl_index) =
            Self::build_trait_indices(loaded_modules);
        Self {
            type_table,
            symbols,
            loaded_modules,
            newtypes: IndexMap::new(),
            struct_fields: IndexMap::new(),
            variant_cases: IndexMap::new(),
            enum_cases: IndexMap::new(),
            flags_cases: IndexMap::new(),
            resource_types: IndexMap::new(),
            all_newtypes: IndexMap::new(),
            all_struct_fields: IndexMap::new(),
            all_variant_cases: IndexMap::new(),
            all_enum_cases: IndexMap::new(),
            all_flags_cases: IndexMap::new(),
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
            trait_impl_index,
            trait_decl_index,
            blanket_trait_impl_index,
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

                    // Register explicit type params from impl<T: Bound> declarations,
                    // skipping concrete types (e.g., `impl<i32, T>` — skip "i32").
                    // This handles both `impl<T> Trait for Struct<T>` and
                    // `impl<T: Bound> OtherTrait for T` (T is the impl type directly).
                    let mut actual_idx = 0u32;
                    for param in &impl_block.type_params {
                        if self.is_known_type_name(&param.name) {
                            // Concrete type in explicit params (e.g., `impl<i32, T>`): skip
                            if !param.bounds.is_empty() {
                                self.current_type_param_bounds
                                    .entry(param.name.clone())
                                    .or_insert_with(Vec::new)
                                    .extend(param.bounds.iter().map(|b| b.name.clone()));
                            }
                            continue;
                        }
                        if !self.current_type_params.contains_key(&param.name) {
                            let type_id = self
                                .type_table
                                .borrow_mut()
                                .make_type_param(param.name.clone(), actual_idx);
                            self.current_type_params
                                .insert(param.name.clone(), (actual_idx, type_id));
                        }
                        if !param.bounds.is_empty() {
                            self.current_type_param_bounds
                                .entry(param.name.clone())
                                .or_insert_with(Vec::new)
                                .extend(param.bounds.iter().map(|b| b.name.clone()));
                        }
                        actual_idx += 1;
                    }

                    if let ast::Type::Generic(generic) = &impl_block.ty {
                        for (i, arg) in generic.args.iter().enumerate() {
                            if let ast::Type::Named(named) = arg {
                                let name = &named.name;
                                if !self.current_type_params.contains_key(name)
                                    && !self.is_known_type_name(name)
                                {
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
                Item::Flags(flags_decl) => {
                    if let Some(flags_info) = self.flags_cases.get(&flags_decl.name) {
                        let tir_flags = TirFlags {
                            name: flags_decl.name.clone(),
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
                Item::Type(newtype_decl) => {
                    if let Some(&type_id) = self.newtypes.get(&newtype_decl.name) {
                        tir_module.add_newtype(TirNewtype {
                            name: newtype_decl.name.clone(),
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

        // Share the type table via Rc::clone
        tir_module.type_table = Rc::clone(&self.type_table);

        // Preserve data section
        if let Some(data) = module.data_section() {
            tir_module = tir_module.with_data_section(Some(data.to_string()));
        }

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
