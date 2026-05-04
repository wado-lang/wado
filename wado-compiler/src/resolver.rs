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
mod callee;
mod closure;
mod coercion;
mod expr;
mod handlers;
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
use crate::symbol::{Symbol, SymbolKey, SymbolTable};
use crate::tir::{
    self as tir, TirEnum, TirEnumCase, TirFlags, TirFlagsMember, TirModule, TirNewtype, TypeId,
    TypeTable,
};

use trait_env::TraitEnv;
pub use types::TypeError;
use types::{
    EnumInfo, FlagsInfo, GenericNewtypeInfo, ResourceInfo, StructFieldInfo, TypeLookup, VariantInfo,
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
    /// Per-module nested maps for cross-module type resolution (shared via Rc).
    /// These are the *single source of truth* for declared type info; all
    /// per-module name resolution is performed by walking these via
    /// [`TypeLookup`] without cloning their contents into per-module flat maps.
    all_newtypes: Rc<IndexMap<ModuleSource, IndexMap<String, TypeId>>>,
    all_generic_newtypes: Rc<IndexMap<ModuleSource, IndexMap<String, GenericNewtypeInfo>>>,
    all_struct_fields: Rc<IndexMap<ModuleSource, IndexMap<String, StructFieldInfo>>>,
    all_variant_cases: Rc<IndexMap<ModuleSource, IndexMap<String, VariantInfo>>>,
    all_enum_cases: Rc<IndexMap<ModuleSource, IndexMap<String, EnumInfo>>>,
    all_flags_cases: Rc<IndexMap<ModuleSource, IndexMap<String, FlagsInfo>>>,
    all_resource_types: Rc<IndexMap<ModuleSource, IndexMap<String, ResourceInfo>>>,
    /// Per-module import context (consumed by [`TypeLookup`]). Built once per
    /// module from `use` declarations; replaces the 7 flat maps the resolver
    /// used to clone for each module.
    imported_type_sources: IndexMap<String, ModuleSource>,
    import_original_names: IndexMap<String, String>,
    /// Locally discovered additions to the type tables. Anonymous structs
    /// (synthesized from struct literals) and the resolver's own walk of
    /// `module.items` insert here so [`TypeLookup`] can find them at the
    /// highest priority. Always empty unless the current module has a
    /// reason to override / extend the shared tables.
    local_struct_fields: IndexMap<String, StructFieldInfo>,
    local_newtypes: IndexMap<String, TypeId>,
    local_generic_newtypes: IndexMap<String, GenericNewtypeInfo>,
    local_enum_cases: IndexMap<String, EnumInfo>,
    local_flags_cases: IndexMap<String, FlagsInfo>,
    local_variant_cases: IndexMap<String, VariantInfo>,
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
    /// Built from import declarations and local interface declarations.
    effect_sources: IndexMap<String, ModuleSource>,
    /// Effect parameter names currently in scope (from enclosing function's `<effect E>`)
    current_effect_params: IndexSet<String>,
    /// Map from effect parameter name to its declaration `AstId` in the current scope.
    /// Used to record use→def edges for effect parameter references in `with` clauses.
    current_effect_param_decls: IndexMap<String, crate::ast::AstId>,
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
    /// Use→def map for local variables. Shared via `Rc<RefCell<…>>` with
    /// [`crate::resolver::orchestration::AnnotateState`] so LSP queries see
    /// references as soon as the resolver has walked the body.
    references: Rc<RefCell<IndexMap<SymbolKey, SymbolKey>>>,
    /// Local binding [`Symbol`]s emitted alongside `references`. Populated at
    /// every `ctx.add_local(...)` call that carries a user-visible defining
    /// `AstId`. Shared via `Rc<RefCell<…>>` with `AnnotateState`.
    local_symbols: Rc<RefCell<IndexMap<SymbolKey, Symbol>>>,
    /// When resolving a default-expression AST at a call site, fall back to
    /// looking up unresolved identifiers in this module's global scope. This
    /// preserves the callee's lexical scope for defaults that reference
    /// module-private items (see WEP 2026-04-11).
    pub(super) default_scope_module: Option<ModuleSource>,
    /// Kiln invocation redirects consulted by `use` resolution sites. Shared
    /// by `Rc` so per-module Resolver instances can read the single
    /// compilation-unit-wide redirect map cheaply.
    pub(super) invocations: Rc<crate::kiln::InvocationIndex>,
    /// `ModuleSource` interner shared with the loader and downstream
    /// phases. Wrapped in `Rc<RefCell<>>` so per-module resolver
    /// instances can `borrow_mut()` it from `&self` contexts (e.g.
    /// `record_use_specifier_references`).
    pub(super) interner: Rc<RefCell<crate::name::ModuleSourceInterner>>,
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
            all_newtypes: Rc::new(IndexMap::default()),
            all_generic_newtypes: Rc::new(IndexMap::default()),
            all_struct_fields: Rc::new(IndexMap::default()),
            all_variant_cases: Rc::new(IndexMap::default()),
            all_enum_cases: Rc::new(IndexMap::default()),
            all_flags_cases: Rc::new(IndexMap::default()),
            all_resource_types: Rc::new(IndexMap::default()),
            imported_type_sources: IndexMap::default(),
            import_original_names: IndexMap::default(),
            local_struct_fields: IndexMap::default(),
            local_newtypes: IndexMap::default(),
            local_generic_newtypes: IndexMap::default(),
            local_enum_cases: IndexMap::default(),
            local_flags_cases: IndexMap::default(),
            local_variant_cases: IndexMap::default(),
            function_return_types: IndexMap::default(),
            imported_functions: IndexSet::default(),
            namespace_imports: IndexMap::default(),
            logger,
            current_module_source: ModuleSource::entry_point_uninitialized(),
            entry_module_source: ModuleSource::entry_point_uninitialized(),
            current_module_items: &[],
            effect_sources: IndexMap::default(),
            current_effect_params: IndexSet::default(),
            current_effect_param_decls: IndexMap::default(),
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
            trait_env,
            included_files,
            known_type_names_cache: IndexSet::default(),
            indexing_trait_cache: IndexMap::default(),
            trait_check_stack: RefCell::new(Vec::new()),
            method_info_cache: IndexMap::default(),
            pending_anonymous_structs: Vec::new(),
            current_module_func_index: IndexMap::default(),
            loaded_module_func_indices: IndexMap::default(),
            references: Rc::new(RefCell::new(IndexMap::default())),
            local_symbols: Rc::new(RefCell::new(IndexMap::default())),
            default_scope_module: None,
            invocations: Rc::new(crate::kiln::InvocationIndex::new()),
            interner: Rc::new(RefCell::new(crate::name::ModuleSourceInterner::new())),
        }
    }

    /// Construct a [`TypeLookup`] view over the resolver's current import
    /// context and shared `all_*` tables. Use this for any type-name
    /// resolution; never reach into `all_*` directly.
    pub(crate) fn type_lookup(&self) -> TypeLookup<'_> {
        TypeLookup {
            current_module_source: &self.current_module_source,
            imported_type_sources: &self.imported_type_sources,
            import_original_names: &self.import_original_names,
            all_newtypes: &self.all_newtypes,
            all_struct_fields: &self.all_struct_fields,
            all_variant_cases: &self.all_variant_cases,
            all_enum_cases: &self.all_enum_cases,
            all_flags_cases: &self.all_flags_cases,
            all_resource_types: &self.all_resource_types,
            all_generic_newtypes: &self.all_generic_newtypes,
            local_struct_fields: &self.local_struct_fields,
            local_newtypes: &self.local_newtypes,
            local_enum_cases: &self.local_enum_cases,
            local_flags_cases: &self.local_flags_cases,
            local_generic_newtypes: &self.local_generic_newtypes,
            local_variant_cases: &self.local_variant_cases,
        }
    }

    pub(super) fn lookup_struct_fields(&self, name: &str) -> Option<&StructFieldInfo> {
        self.type_lookup().struct_fields(name)
    }

    pub(super) fn lookup_variant_case(&self, name: &str) -> Option<&VariantInfo> {
        self.type_lookup().variant_case(name)
    }

    pub(super) fn lookup_enum_case(&self, name: &str) -> Option<&EnumInfo> {
        self.type_lookup().enum_case(name)
    }

    pub(super) fn lookup_flags_case(&self, name: &str) -> Option<&FlagsInfo> {
        self.type_lookup().flags_case(name)
    }

    pub(super) fn lookup_resource_type(&self, name: &str) -> Option<&ResourceInfo> {
        self.type_lookup().resource_type(name)
    }

    pub(super) fn lookup_generic_newtype(&self, name: &str) -> Option<&GenericNewtypeInfo> {
        self.type_lookup().generic_newtype(name)
    }

    pub(super) fn lookup_newtype(&self, name: &str) -> Option<TypeId> {
        self.type_lookup().newtype(name)
    }

    pub(super) fn contains_variant(&self, name: &str) -> bool {
        self.lookup_variant_case(name).is_some()
    }

    /// Run `body` with the resolver's "current module" perspective swapped to
    /// `module_source` and the supplied import context. Locals are cleared
    /// because they describe in-progress resolution, not the target module's
    /// pre-existing definitions; they are restored on return.
    ///
    /// Replaces the legacy pattern of cloning per-module flat maps via
    /// `build_module_map` and swapping six fields in/out.
    pub(super) fn with_module_perspective<R>(
        &mut self,
        module_source: ModuleSource,
        imported_type_sources: IndexMap<String, ModuleSource>,
        import_original_names: IndexMap<String, String>,
        body: impl FnOnce(&mut Self) -> R,
    ) -> R {
        let saved_src = std::mem::replace(&mut self.current_module_source, module_source);
        let saved_imp = std::mem::replace(&mut self.imported_type_sources, imported_type_sources);
        let saved_orig = std::mem::replace(&mut self.import_original_names, import_original_names);
        let saved_local_struct = std::mem::take(&mut self.local_struct_fields);
        let saved_local_newtypes = std::mem::take(&mut self.local_newtypes);
        let saved_local_enum = std::mem::take(&mut self.local_enum_cases);
        let saved_local_flags = std::mem::take(&mut self.local_flags_cases);
        let saved_local_gnt = std::mem::take(&mut self.local_generic_newtypes);
        let saved_local_variant = std::mem::take(&mut self.local_variant_cases);

        let result = body(self);

        self.current_module_source = saved_src;
        self.imported_type_sources = saved_imp;
        self.import_original_names = saved_orig;
        self.local_struct_fields = saved_local_struct;
        self.local_newtypes = saved_local_newtypes;
        self.local_enum_cases = saved_local_enum;
        self.local_flags_cases = saved_local_flags;
        self.local_generic_newtypes = saved_local_gnt;
        self.local_variant_cases = saved_local_variant;
        result
    }

    /// Record that an identifier resolved to a local binding in the current
    /// module. Both `use_id` and `def_id` live in `current_module_source`.
    pub(super) fn record_reference(&self, use_id: crate::ast::AstId, def_id: crate::ast::AstId) {
        let use_key = SymbolKey::new(self.current_module_source.clone(), use_id);
        let def_key = SymbolKey::new(self.current_module_source.clone(), def_id);
        self.references.borrow_mut().insert(use_key, def_key);
    }

    /// Record a use→def reference when the definition lives in a (possibly
    /// different) module identified directly by a [`SymbolKey`]. Prefer
    /// [`Self::record_reference_to_decl`] when the call site is constructing
    /// the key from `(module, ast_id)` parts.
    pub(super) fn record_reference_to_key(&self, use_id: crate::ast::AstId, def_key: SymbolKey) {
        let use_key = SymbolKey::new(self.current_module_source.clone(), use_id);
        self.references.borrow_mut().insert(use_key, def_key);
    }

    /// Record a use→def reference where the defining declaration is
    /// identified by its `(module, ast_id)` pair. Convenience over
    /// [`Self::record_reference_to_key`] for call sites that would
    /// otherwise construct a [`SymbolKey`] inline — keeps `SymbolKey::new`
    /// confined to a single place.
    pub(super) fn record_reference_to_decl(
        &self,
        use_id: crate::ast::AstId,
        decl_module: &ModuleSource,
        decl_ast_id: crate::ast::AstId,
    ) {
        self.record_reference_to_key(use_id, SymbolKey::new(decl_module.clone(), decl_ast_id));
    }

    /// Record that an identifier resolved to a declared symbol reachable from
    /// the current module under `name` (local item, imported item, imported
    /// namespace member, etc.). Looks up the defining [`SymbolKey`] through
    /// the symbol table; no-op if the name is not declared.
    pub(super) fn record_item_reference_by_name(&self, use_id: crate::ast::AstId, name: &str) {
        let Some(sym) = self
            .symbols
            .lookup_in_module(&self.current_module_source, name)
            .or_else(|| self.symbols.lookup(name))
        else {
            return;
        };
        let use_key = SymbolKey::new(self.current_module_source.clone(), use_id);
        self.references
            .borrow_mut()
            .insert(use_key, sym.defined_at.clone());
    }

    /// Record use→def edges for a `TypeName::CaseName` qualified path
    /// expression. The prefix segment (`TypeName`) is resolved by name in
    /// the current module's scope; the suffix segment (`CaseName`) points
    /// directly at `(case_module, case_ast_id)`.
    ///
    /// Used for variant cases, enum cases, and flags members reached via
    /// a two-segment qualified ident.
    pub(super) fn record_qualified_case(
        &self,
        ident: &crate::ast::IdentExpr,
        type_name: &str,
        case_module: &ModuleSource,
        case_ast_id: crate::ast::AstId,
    ) {
        if let Some(prefix_seg) = ident.segments.first() {
            self.record_item_reference_by_name(prefix_seg.id, type_name);
        }
        if let Some(suffix_seg) = ident.segments.get(1) {
            self.record_reference_to_decl(suffix_seg.id, case_module, case_ast_id);
        }
    }

    /// Record the suffix (`Case`) segment of a `ns::Type::Case`
    /// namespace-qualified case path. The leading `ns` and `Type`
    /// segments are left to existing namespace-import edges.
    pub(super) fn record_namespaced_case(
        &self,
        ident: &crate::ast::IdentExpr,
        case_module: &ModuleSource,
        case_ast_id: crate::ast::AstId,
    ) {
        if let Some(seg) = ident.segments.get(2) {
            self.record_reference_to_decl(seg.id, case_module, case_ast_id);
        }
    }

    /// Record a use→def edge from `use_id` to `def_id` (in the current
    /// module) when the defining id is known. Convenience for sites that
    /// receive an `Option<AstId>` from a local variable lookup.
    pub(super) fn record_reference_opt(
        &self,
        use_id: crate::ast::AstId,
        def_id: Option<crate::ast::AstId>,
    ) {
        if let Some(def_id) = def_id {
            self.record_reference(use_id, def_id);
        }
    }

    /// Record a use→def edge for a type-name reference (`Type::Named` /
    /// `Type::Generic`). Generic-parameter names in scope (e.g. `T` in
    /// `fn f<T>(x: T)`) win over module-level items: jump-to-def lands on
    /// the `<T>` declaration rather than on a top-level item that happens
    /// to share the name. Falls through to the symbol-table lookup
    /// otherwise.
    pub(super) fn record_type_name_reference(&self, use_id: crate::ast::AstId, name: &str) {
        if let Some(&decl_id) = self.trait_ctx.type_param_decls.get(name) {
            self.record_reference(use_id, decl_id);
        } else {
            self.record_item_reference_by_name(use_id, name);
        }
    }

    /// Look up the `AstId` of an impl-block method by its defining module, the
    /// receiving type name, and the method name. Returns `None` if the module
    /// is not loaded or no matching method exists. Searches all impl blocks in
    /// the module; for trait methods prefers the most specific match (struct +
    /// method; trait name is not disambiguated here).
    pub(super) fn find_impl_method_ast_id(
        &self,
        module_source: &ModuleSource,
        struct_name: &str,
        method_name: &str,
    ) -> Option<crate::ast::AstId> {
        let items: &[Item] = if module_source == &self.current_module_source {
            self.current_module_items
        } else {
            self.loaded_modules.get(module_source)?.items.as_slice()
        };
        for item in items {
            if let Item::Impl(impl_block) = item
                && Self::get_type_name_static(&impl_block.ty) == struct_name
            {
                for method in &impl_block.methods {
                    if method.name == method_name {
                        return Some(method.id);
                    }
                }
            }
        }
        None
    }

    /// Record a local binding's [`Symbol`] so that LSP hover on a use site can
    /// retrieve the defining name / mutability. Called at each site where a
    /// user-visible local is introduced.
    pub(super) fn record_local_symbol(
        &self,
        def_id: crate::ast::AstId,
        name: &str,
        span: crate::token::Span,
        is_mut: bool,
    ) {
        let key = SymbolKey::new(self.current_module_source.clone(), def_id);
        let symbol = Symbol {
            name: name.to_string(),
            kind: crate::symbol::SymbolKind::Variable(crate::symbol::VariableSymbol {
                is_mut,
                is_reactive: false,
            }),
            defined_at: key.clone(),
            span: Some(span),
        };
        self.local_symbols.borrow_mut().insert(key, symbol);
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
    /// Disambiguates same-named structs across modules by also matching the
    /// owning `module_source`. Falls back to scanning the shared `all_*`
    /// table when the visible projection (current module + locally created
    /// anonymous structs) doesn't have the entry.
    pub(super) fn lookup_struct_fields_in(
        &self,
        name: &str,
        module_source: &ModuleSource,
    ) -> Option<&StructFieldInfo> {
        self.local_struct_fields
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
        interner: &mut crate::name::ModuleSourceInterner,
        module: &Module,
        module_source: &ModuleSource,
        invocations: &crate::kiln::InvocationIndex,
    ) -> IndexMap<String, ModuleSource> {
        let mut sources = IndexMap::default();
        for item in &module.items {
            match item {
                Item::Use(use_decl) => {
                    let source = name::resolve_import_with_invocations(
                        interner,
                        module_source,
                        &use_decl.source,
                        None,
                        invocations,
                    );
                    for use_item in &use_decl.items {
                        match use_item {
                            ast::UseItem::InterfaceFunctions { interface_name, .. } => {
                                sources.insert(interface_name.clone(), source.clone());
                            }
                            ast::UseItem::Simple { name, alias, .. } => {
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
                Item::Interface(effect_decl) => {
                    sources.insert(effect_decl.name.clone(), module_source.clone());
                }
                Item::Resource(resource_decl) => {
                    sources.insert(resource_decl.name.clone(), module_source.clone());
                }
                _ => {}
            }
        }
        sources
    }

    /// Resolve AST effect names (strings) to TIR `EffectRefs` with module source information.
    ///
    /// `effect_ids` is a parallel slice with `(AstId, Span)` of each effect-name identifier
    /// occurrence. When non-empty, use→def edges are recorded so LSP jump-to-definition
    /// works on effect references in `with` clauses. An empty slice skips recording
    /// (used by synthetic/internal effect lists with no source identifiers).
    pub(crate) fn resolve_effects(
        &self,
        effects: &[String],
        effect_ids: &[(crate::ast::AstId, crate::token::Span)],
    ) -> Vec<tir::EffectRef> {
        effects
            .iter()
            .enumerate()
            .map(|(i, name)| {
                let use_id = effect_ids.get(i).map(|(id, _)| *id);
                if let Some(&decl_id) = self.current_effect_param_decls.get(name) {
                    if let Some(use_id) = use_id {
                        self.record_reference(use_id, decl_id);
                    }
                    tir::EffectRef::Param { name: name.clone() }
                } else if let Some(source) = self.effect_sources.get(name) {
                    // Canonicalize: re-exports point at the importing module, but
                    // identity must match the defining module so two `with Stdout`
                    // clauses (one importing from `core:cli`, one from `wasi:cli`)
                    // refer to the same effect.
                    let canonical = self
                        .symbols
                        .lookup_in_module(source, name)
                        .map(|sym| {
                            if let Some(use_id) = use_id {
                                self.record_reference_to_key(use_id, sym.defined_at.clone());
                            }
                            sym.defined_at.module.clone()
                        })
                        .unwrap_or_else(|| source.clone());
                    tir::EffectRef::Concrete {
                        name: name.clone(),
                        module_source: canonical,
                    }
                } else {
                    if let Some(use_id) = use_id {
                        self.record_item_reference_by_name(use_id, name);
                    }
                    // Fallback: resolve via the import-aware symbol table so that
                    // prelude-defined effects/resources (e.g. `Future`, `Stream`)
                    // canonicalise to their defining module rather than the
                    // current module. Falls through to `current_module_source`
                    // only when no symbol exists (genuinely-local declaration).
                    let canonical = self
                        .symbols
                        .lookup(name)
                        .map(|sym| {
                            if let Some(use_id) = use_id {
                                self.record_reference_to_key(use_id, sym.defined_at.clone());
                            }
                            sym.defined_at.module.clone()
                        })
                        .unwrap_or_else(|| self.current_module_source.clone());
                    tir::EffectRef::Concrete {
                        name: name.clone(),
                        module_source: canonical,
                    }
                }
            })
            .collect()
    }

    /// Record use→def edges for each imported name in `use { a, b as c } from "..."`
    /// declarations. The cursor landing on an imported name inside a `use`
    /// specifier list should jump to the defining symbol in the source module.
    fn record_use_specifier_references(&self, module: &Module) {
        for item in &module.items {
            let Item::Use(use_decl) = item else { continue };
            let source = name::resolve_import_with_invocations(
                &mut self.interner.borrow_mut(),
                &self.current_module_source,
                &use_decl.source,
                Some(&self.entry_module_source),
                &self.invocations,
            );
            for use_item in &use_decl.items {
                match use_item {
                    ast::UseItem::Simple { id, name, .. } => {
                        if let Some(sym) = self.symbols.lookup_in_module(&source, name) {
                            self.record_reference_to_key(*id, sym.defined_at.clone());
                        }
                    }
                    ast::UseItem::InterfaceFunctions { .. }
                    | ast::UseItem::Wildcard
                    | ast::UseItem::Namespace { .. } => {}
                }
            }
        }
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
        self.effect_sources = Self::build_effect_sources(
            &mut self.interner.borrow_mut(),
            module,
            &module_source,
            &self.invocations,
        );

        // Record use→def edges for names that appear inside `use { ... }` specifiers.
        // These power LSP jump-to-definition when the cursor is on an imported
        // name in the `use` declaration itself.
        self.record_use_specifier_references(module);

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
                let source_module_source = name::resolve_import_with_invocations(
                    &mut self.interner.borrow_mut(),
                    &module_source,
                    &use_decl.source,
                    None,
                    &self.invocations,
                );

                // Look up the source module to find global declarations
                if let Some(source_module) = self.loaded_modules.get(&source_module_source) {
                    for use_item in &use_decl.items {
                        if let ast::UseItem::Simple { name, alias, .. } = use_item {
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
                            impl_block.trait_type.as_ref(),
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
                    // not explicitly provided in the impl block. `trait_name`
                    // carries the mangled form ("Maker<i32>") used for method
                    // naming; the trait declaration itself is indexed by its
                    // base name ("Maker"), so we derive that from the AST.
                    if let (Some(trait_n), Some(trait_ast)) =
                        (trait_name.as_ref(), impl_block.trait_type.as_ref())
                    {
                        let trait_decl_name = self.get_type_name(trait_ast);
                        let default_methods: Vec<ast::Function> = self
                            .find_trait_decl_methods(&trait_decl_name)
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
                                Some(trait_ast),
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
                    if let Some(flags_info) = self.lookup_flags_case(&flags_decl.name) {
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
                    if let Some(type_id) = self.lookup_newtype(&newtype_decl.name) {
                        tir_module.add_newtype(TirNewtype {
                            name: newtype_decl.name.clone(),
                            module_source: self.current_module_source.clone(),
                            is_pub: newtype_decl.is_pub,
                            type_id,
                            span: newtype_decl.span,
                        });
                    }
                }
                Item::Interface(effect_decl) => {
                    let tir_effect = self.resolve_effect_decl(effect_decl);
                    tir_module.add_effect(tir_effect);
                }
                Item::Resource(resource_decl) => {
                    let tir_resource = self.resolve_resource_decl(resource_decl);
                    tir_module.add_resource(tir_resource);
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
