//! Type resolution phase for Wado
//!
//! The type elaborator:
//! 1. Takes the parsed AST and symbol table from the analyzer
//! 2. Performs type inference and type checking
//! 3. Produces the Typed Intermediate Representation (TIR)
//!
//! All type resolution happens in this phase. The output TIR has fully
//! resolved types on every expression, making code generation mechanical.

pub(crate) mod assert;
mod call;
mod callee;
mod closure;
mod coercion;
mod control_flow;
mod expr;
mod handlers;
mod infer;
mod infer_hole;
mod instantiate;
mod item;
pub(crate) mod liveness;
mod matches;
mod method_call;
mod method_lookup;
mod module;
mod operators;
pub(crate) mod orchestration;
mod reflect;
pub(crate) mod reify;
mod scope;
pub(crate) mod sem;
pub(crate) mod sig;
mod stmt;
mod synth;
mod template;
pub(crate) mod trait_env;
mod trait_query;
mod type_resolution;
mod typecheck;
pub(crate) mod types;
mod tysys;
mod util;
mod written;

use std::cell::RefCell;
use std::rc::Rc;

use crate::hashmap::IndexMap;

use crate::ast::{self, Item, Module};
use crate::compiler_host::CompilerHost;
use crate::logger::Logger;
use crate::module_source::{ModuleSource, ModuleSourceInterner};
use crate::name::{self as name, Receiver, RefKind};
use crate::symbol::{Symbol, SymbolTable};
use crate::tir::{self as tir, TypeId, TypeTable};

/// Build a function-name → item-index map for a module's items. Used
/// once per loaded module during annotate
/// ([`orchestration::Elaborator::annotate_modules`]) to populate
/// [`tysys::TypeSystem::loaded_module_func_indices`]; the per-module
/// body walk then consults that pre-built index instead of rebuilding
/// here.
pub(crate) fn build_func_index(items: &[Item]) -> IndexMap<String, usize> {
    let mut index = IndexMap::default();
    for (i, item) in items.iter().enumerate() {
        if let Item::Function(func) = item {
            index.insert(func.name.clone(), i);
        }
    }
    index
}

pub use types::TypeError;
use types::{
    EnumInfo, FlagsInfo, GenericNewtypeInfo, ResourceInfo, StructFieldInfo, TypeLookup, VariantInfo,
};

pub struct Elaborator<'a, H: CompilerHost> {
    /// Pipeline-wide type knowledge: type arena, decl-interned type
    /// tables, registries, included-files map, and the read-only caches
    /// built once during `annotate_modules`. See [`tysys::TypeSystem`].
    pub(crate) tysys: tysys::TypeSystem,
    /// Per-module semantic facts (imports, decls, bindings, type
    /// annotations). The elaborator takes ownership of one
    /// [`sem::ModuleSemantics`] at the start of each per-module pass
    /// ([`Self::annotate_module_decls`], [`Self::annotate_module_bodies`])
    /// and the driver re-installs it into
    /// [`orchestration::AnnotateState::module_semantics`] afterwards. See
    /// the [`sem`] module-level documentation for the membership rules.
    pub(crate) sem: sem::ModuleSemantics,
    /// Symbol table from analyzer
    symbols: &'a SymbolTable,
    /// Logger for emitting diagnostics
    logger: &'a Logger<'a, H>,
    /// Current module source being resolved (for struct type `module_source`).
    /// Identifies the active `ModuleSemantics`, which the driver swaps by
    /// `IndexMap<ModuleSource, ModuleSemantics>` key.
    current_module_source: ModuleSource,
    /// Entry module source (for cross-module import dedup)
    entry_module_source: ModuleSource,
    /// Transient annotate-time scope: trait-resolution context (incl.
    /// effect params), `type_implements_trait` recursion guard, and the
    /// default-expression module fallback. Mutated only through the RAII
    /// guards in [`scope`]; see [`scope::Scope`].
    annotate_ctx: scope::Scope,
    /// Kiln invocation redirects consulted by `use` resolution sites. Shared
    /// by `Rc` so per-module Elaborator instances can read the single
    /// compilation-unit-wide redirect map cheaply.
    pub(super) invocations: Rc<crate::kiln::InvocationIndex>,
    /// `ModuleSource` interner shared with the loader and downstream
    /// phases. Wrapped in `Rc<RefCell<>>` so per-module elaborator
    /// instances can `borrow_mut()` it from `&self` contexts (e.g.
    /// `record_use_specifier_references`).
    pub(super) interner: Rc<RefCell<ModuleSourceInterner>>,
    /// When `true`, the single use→def edge sink [`Self::insert_reference`]
    /// (which every `record_*` helper funnels through) drops edges instead of
    /// recording them.
    ///
    /// One caller: argument classification
    /// ([`Self::synthesize_arg_class`]), which walks an argument
    /// *speculatively* to pick among overloads and must leave no trace — the
    /// real walk of the same node records the authoritative edge once the
    /// callee is chosen.
    pub(super) suppress_reference_recording: bool,
    /// Per-module deferred-inference state, solved and swept in
    /// [`Self::finalize_infer_holes`] at the end of the module walk. See
    /// [`infer_hole`].
    pub(super) infer_holes: infer_hole::InferHoleTable,
    /// The `(base, assoc)` pairs whose binding is being resolved right now.
    /// Two assoc types bounded through each other have no fixpoint, so a pair
    /// already on the walk contributes no binding and stays abstract.
    pub(super) assoc_binding_stack: crate::hashmap::IndexSet<(crate::tir::TypeId, String)>,
}

impl<H: CompilerHost> scope::TypeParamScope<'_, '_, H> {
    /// Register an `impl` block's own type parameters into this scope,
    /// numbering them into the positional slots the block's methods
    /// resolve against.
    ///
    /// Shared by the decl pass (which records each method's canonical
    /// signature) and the body walk, so both see the same slots.
    pub(super) fn register_impl_block_params(&mut self, impl_block: &ast::ImplBlock) {
        let mut actual_idx = 0u32;
        for param in &impl_block.type_params {
            if self
                .tysys
                .is_known_type_name_in(&self.current_module_source, &param.name)
            {
                // Concrete type in explicit params (e.g., `impl<i32, T>`): skip
                if !param.bounds.is_empty() {
                    self.annotate_ctx
                        .trait_ctx
                        .type_param_bounds
                        .entry(param.name.clone())
                        .or_default()
                        .extend(param.bounds.clone());
                }
                continue;
            }
            if !self
                .annotate_ctx
                .trait_ctx
                .type_params
                .contains_key(&param.name)
            {
                let type_id = if param.is_pack {
                    self.tysys
                        .type_table
                        .borrow_mut()
                        .make_type_pack(param.name.clone(), actual_idx)
                } else {
                    self.tysys
                        .type_table
                        .borrow_mut()
                        .make_type_param(param.name.clone(), actual_idx)
                };
                self.annotate_ctx
                    .trait_ctx
                    .type_params
                    .insert(param.name.clone(), (actual_idx, type_id));
            }
            if !param.bounds.is_empty() {
                self.annotate_ctx
                    .trait_ctx
                    .type_param_bounds
                    .entry(param.name.clone())
                    .or_default()
                    .extend(param.bounds.clone());
            }
            actual_idx += 1;
        }

        // Unwrap reference for ref-type impls (impl Trait for &Container<T>)
        let impl_inner_ty = match &impl_block.ty {
            ast::Type::Reference(inner) | ast::Type::MutReference(inner) => inner.as_ref(),
            other => other,
        };
        if let ast::Type::Generic(generic) = impl_inner_ty {
            for (i, arg) in generic.args.iter().enumerate() {
                if let ast::Type::Named(named) = arg {
                    let name = &named.name;
                    if !self.annotate_ctx.trait_ctx.type_params.contains_key(name)
                        && !self
                            .tysys
                            .is_known_type_name_in(&self.current_module_source, name)
                    {
                        let type_id = self
                            .tysys
                            .type_table
                            .borrow_mut()
                            .make_type_param(name.clone(), i as u32);
                        self.annotate_ctx
                            .trait_ctx
                            .type_params
                            .insert(name.clone(), (i as u32, type_id));
                    }
                }
            }
        }
    }
}
impl<'a, H: CompilerHost> Elaborator<'a, H> {
    /// Emit a `TypeError` attributed to `current_module_source` — the channel
    /// for every diagnostic raised during item/body resolution.
    pub(super) fn emit(
        &self,
        err: impl Into<crate::compiler_host::Diagnostic>,
    ) -> Result<(), crate::logger::Bail> {
        self.logger.error_in(&self.current_module_source, err)
    }

    /// [`Self::emit`] for a diagnostic whose span belongs to `module` — a
    /// foreign default expression or associated-constant body — so the file it
    /// is reported against is the one the span indexes.
    pub(super) fn emit_in(
        &self,
        module: &ModuleSource,
        err: impl Into<crate::compiler_host::Diagnostic>,
    ) -> Result<(), crate::logger::Bail> {
        self.logger.error_in(module, err)
    }

    /// The declaration an item node declares.
    ///
    /// Every item the collect pass walks was declared into the table, so a miss
    /// is a hole in that pass rather than a name that reached nothing.
    pub(super) fn def_of_item(&self, id: crate::ast::AstId) -> crate::defs::DefId {
        self.tysys
            .resolutions
            .defs()
            .of_ast_id(id)
            .expect("an item declaration has an identity")
    }

    /// The symbol `name` reaches from `module`, for a caller whose reference site
    /// is not at hand — a mangled name, a synthesis target. No scope is run.
    pub(crate) fn symbol_named(
        &self,
        module: &ModuleSource,
        name: &str,
    ) -> Option<&'a crate::symbol::Symbol> {
        // Three recorded facts, in the order the scope stores them and none of
        // them a walk: what this module `use`d under the name, what it declares
        // itself, and what the prelude puts in scope everywhere. No spelling
        // another module happens to share can steer any of them.
        if let Some(def) = self.tysys.resolutions.imported_as(module, name) {
            return self.symbols.get(&self.tysys.resolutions.defs().ast_id(def));
        }
        if let Some(symbol) = self.symbols.lookup_in_module(module, name) {
            return Some(symbol);
        }
        let def = self.tysys.resolutions.prelude_decl(name)?;
        self.symbols.get(&self.tysys.resolutions.defs().ast_id(def))
    }

    /// Construct a [`TypeLookup`] view over the elaborator's current import
    /// context and shared `all_*` tables. Use this for any type-name
    /// resolution; never reach into `all_*` directly.
    pub(crate) fn type_lookup(&self) -> TypeLookup<'_> {
        TypeLookup {
            current_module_source: &self.current_module_source,
            resolutions: &self.tysys.resolutions,
            namespace_imports: &self.sem.imports.namespace_imports,
            all_newtypes: &self.tysys.all_newtypes,
            all_struct_fields: &self.tysys.all_struct_fields,
            all_variant_cases: &self.tysys.all_variant_cases,
            all_enum_cases: &self.tysys.all_enum_cases,
            all_flags_cases: &self.tysys.all_flags_cases,
            all_resource_types: &self.tysys.all_resource_types,
            all_generic_newtypes: &self.tysys.all_generic_newtypes,
            local_struct_fields: &self.sem.decls.local_struct_fields,
            local_newtypes: &self.sem.decls.local_newtypes,
            local_enum_cases: &self.sem.decls.local_enum_cases,
            local_flags_cases: &self.sem.decls.local_flags_cases,
            local_generic_newtypes: &self.sem.decls.local_generic_newtypes,
            local_variant_cases: &self.sem.decls.local_variant_cases,
            anon_struct_fields: &self.sem.decls.anon_struct_fields,
            fn_local_items: &self.sem.decls.fn_local_items,
            decls: Some(&self.tysys.trait_env),
        }
    }

    /// Canonicalize a `<ns>::<member>` reference — one `::`, the prefix a
    /// namespace import alias — to bare `<member>`. `None` for any other shape,
    /// multi-segment `<ns>::<Type>::<case>` included. The AST keeps what the user
    /// wrote so LSP cursors land on it, while name lookups see the canonical
    /// form their registries were populated with.
    pub(super) fn strip_ns_prefix<'s>(&self, name: &'s str) -> Option<&'s str> {
        self.sem.imports.strip_ns_prefix(name)
    }

    /// The cases of the variant a written qualifier names — see
    /// [`super::types::TypeLookup::variant_cases_at`].
    pub(super) fn lookup_variant_cases_at(
        &self,
        site: Option<crate::ast::AstId>,
        name: &str,
    ) -> Option<&VariantInfo> {
        self.type_lookup().variant_cases_at(site, name)
    }

    pub(super) fn lookup_flags_members_at(
        &self,
        site: Option<crate::ast::AstId>,
        name: &str,
    ) -> Option<&FlagsInfo> {
        self.type_lookup().flags_members_at(site, name)
    }

    pub(super) fn lookup_newtype(&self, name: &str) -> Option<TypeId> {
        self.type_lookup().newtype(name)
    }

    pub(super) fn lookup_variant_case_of_decl(
        &self,
        def: crate::defs::DefId,
    ) -> Option<&VariantInfo> {
        self.type_lookup().variant_cases_of(def)
    }

    pub(super) fn lookup_enum_case_of_decl(&self, def: crate::defs::DefId) -> Option<&EnumInfo> {
        self.type_lookup().enum_cases_of(def)
    }

    pub(super) fn lookup_resource_type_of_decl(
        &self,
        def: crate::defs::DefId,
    ) -> Option<&ResourceInfo> {
        self.type_lookup().resource_type_of(def)
    }

    pub(super) fn lookup_generic_newtype_of_decl(
        &self,
        def: crate::defs::DefId,
    ) -> Option<&GenericNewtypeInfo> {
        self.type_lookup().generic_newtype_of(def)
    }

    pub(super) fn lookup_newtype_of_decl(&self, def: crate::defs::DefId) -> Option<TypeId> {
        self.type_lookup().newtype_of(def)
    }

    /// The declaration a *type* reference names; see
    /// [`TypeLookup::declaration_at`].
    pub(super) fn type_decl_at(
        &self,
        site: Option<crate::ast::AstId>,
        name: &str,
    ) -> Option<crate::defs::DefId> {
        self.type_lookup().declaration_at(site, name)
    }

    /// Run `body` in `module`'s perspective, swapping the current module and
    /// its namespace imports. For callee-scope work only, such as a parameter
    /// default; already being there skips the swap.
    ///
    /// The walk's own type tables are not swapped with it: they are keyed by
    /// declaration, so an entry answers for the declaration that made it and
    /// for nothing else, whichever module the walk is standing in.
    pub(super) fn with_module_perspective_for<R>(
        &mut self,
        module: &ModuleSource,
        body: impl FnOnce(&mut Self) -> R,
    ) -> R {
        if self.current_module_source == *module {
            return body(self);
        }
        let namespaces = self.tysys.trait_env.namespace_imports(module);
        let saved_src = std::mem::replace(&mut self.current_module_source, module.clone());
        let saved_ns = std::mem::replace(&mut self.sem.imports.namespace_imports, namespaces);

        let result = body(self);

        self.current_module_source = saved_src;
        self.sem.imports.namespace_imports = saved_ns;
        result
    }

    /// Run `body` with use→def reference recording suppressed, restoring the
    /// previous setting on return. See
    /// [`Self::suppress_reference_recording`].
    pub(super) fn with_reference_recording_suppressed<R>(
        &mut self,
        body: impl FnOnce(&mut Self) -> R,
    ) -> R {
        let saved = std::mem::replace(&mut self.suppress_reference_recording, true);
        let result = body(self);
        self.suppress_reference_recording = saved;
        result
    }

    /// The single sink for every use→def edge. All `record_*` helpers funnel
    /// through here, so the [`Self::suppress_reference_recording`] gate lives in
    /// exactly one place: when set, the edge is dropped rather than recorded
    /// as a spurious duplicate by a type-checking query (see the field docs).
    fn insert_reference(&mut self, use_id: crate::ast::AstId, def_id: crate::ast::AstId) {
        if self.suppress_reference_recording {
            return;
        }
        self.sem.bindings.references.insert(use_id, def_id);
    }

    /// Record that an identifier resolved to a local binding in the current
    /// module. Both `use_id` and `def_id` live in `current_module_source`.
    pub(super) fn record_reference(
        &mut self,
        use_id: crate::ast::AstId,
        def_id: crate::ast::AstId,
    ) {
        self.insert_reference(use_id, def_id);
    }

    /// Record a use→def edge to the definition at `def_id`. The defining
    /// node may live in any module — its `AstId` is globally unique, so the
    /// edge needs no module qualifier; navigation recovers the def's module
    /// from the id via [`crate::semantics::Semantics::module_of_id`].
    pub(super) fn record_reference_to_def(
        &mut self,
        use_id: crate::ast::AstId,
        def_id: crate::ast::AstId,
    ) {
        self.insert_reference(use_id, def_id);
    }

    /// The free function the reference site `site` names, answered by the
    /// module that wrote it (WEP 2026-08-12). `None` where it names something
    /// else — a binder, a variant case, a node no walk saw.
    pub(super) fn free_function_at(&self, site: crate::ast::AstId) -> Option<crate::defs::DefId> {
        let def = self.tysys.resolutions.declared_if_walked(site)?;
        (self.tysys.resolutions.defs().kind(def) == crate::defs::DefKind::Function).then_some(def)
    }

    /// The canonical signature of the free function the site names.
    pub(super) fn free_function_sig_at(
        &self,
        site: crate::ast::AstId,
    ) -> Option<&sem::decls::FunctionSig> {
        self.tysys
            .signatures
            .function_sig(self.free_function_at(site)?)
    }

    /// The declaration `id` declares. See [`crate::defs::DefTable::def_at`].
    pub(super) fn def_at(&self, id: crate::ast::AstId) -> crate::defs::DefId {
        self.tysys.resolutions.defs().def_at(id)
    }

    /// The declaration `module` declares under `name`, for the positions no
    /// reference site answers. The module is named by the caller, not searched
    /// for.
    pub(super) fn decl_in_module(
        &self,
        module: &ModuleSource,
        name: &str,
    ) -> Option<crate::defs::DefId> {
        self.symbols
            .lookup_in_module(module, name)
            .and_then(|sym| self.tysys.resolutions.defs().of_ast_id(sym.defined_at))
    }

    /// [`Self::decl_in_module`] as a callee identity.
    fn callee_in_module(&self, module: &ModuleSource, name: &str) -> Option<callee::CalleeRef> {
        Some(self.callee_of(self.decl_in_module(module, name)?))
    }

    /// The callee identity of the declaration `def`.
    fn callee_of(&self, def: crate::defs::DefId) -> callee::CalleeRef {
        callee::CalleeRef::declared(self.tysys.resolutions.defs(), def)
    }

    /// Record a use→def edge naming the declaration `def`. The map is keyed by
    /// node on both sides, so the declaring node is read off the identity here
    /// rather than carried beside it.
    pub(super) fn record_reference_to_decl(
        &mut self,
        use_id: crate::ast::AstId,
        def: crate::defs::DefId,
    ) {
        let node = self.tysys.resolutions.defs().ast_id(def);
        self.insert_reference(use_id, node);
    }

    /// Record that an identifier resolved to a declared symbol reachable from
    /// the current module under `name` (local item, imported item, imported
    /// namespace member, etc.). Looks up the defining [`AstId`](crate::ast::AstId) through
    /// the symbol table; no-op if the name is not declared.
    pub(super) fn record_item_reference_by_name(&mut self, use_id: crate::ast::AstId, name: &str) {
        let Some(sym) = self.symbol_named(&self.current_module_source, name) else {
            return;
        };
        let def_id = sym.defined_at;
        self.insert_reference(use_id, def_id);
    }

    /// Record use→def edges for a `TypeName::CaseName` qualified path
    /// expression. The prefix segment (`TypeName`) is resolved by name in
    /// the current module's scope; the suffix segment (`CaseName`) points
    /// directly at `case_ast_id` (its module is intrinsic to the id).
    ///
    /// Used for variant cases, enum cases, and flags members reached via
    /// a two-segment qualified ident.
    pub(super) fn record_qualified_case(
        &mut self,
        ident: &crate::ast::IdentExpr,
        type_name: &str,
        case_ast_id: crate::ast::AstId,
    ) {
        if let Some(prefix_seg) = ident.segments.first() {
            self.record_item_reference_by_name(prefix_seg.id, type_name);
        }
        if let Some(suffix_seg) = ident.segments.get(1) {
            self.record_reference_to_def(suffix_seg.id, case_ast_id);
        }
    }

    /// Record the suffix (`Case`) segment of a `ns::Type::Case`
    /// namespace-qualified case path. The leading `ns` and `Type`
    /// segments are left to existing namespace-import edges.
    pub(super) fn record_namespaced_case(
        &mut self,
        ident: &crate::ast::IdentExpr,
        case_ast_id: crate::ast::AstId,
    ) {
        if let Some(seg) = ident.segments.get(2) {
            self.record_reference_to_def(seg.id, case_ast_id);
        }
    }

    /// Record a use→def edge from `use_id` to `def_id` (in the current
    /// module) when the defining id is known. Convenience for sites that
    /// receive an `Option<AstId>` from a local variable lookup.
    pub(super) fn record_reference_opt(
        &mut self,
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
    pub(in crate::elaborator) fn record_type_name_reference(
        &mut self,
        use_id: crate::ast::AstId,
        name: &str,
    ) {
        if let Some(&decl_id) = self.annotate_ctx.trait_ctx.type_param_decls.get(name) {
            self.record_reference(use_id, decl_id);
        } else {
            self.record_item_reference_by_name(use_id, name);
        }
    }

    /// Emit `PrivateNamespacedSymbol` when `name` in `target` is out of this
    /// module's reach. A namespace path looks the module up directly, so it
    /// owes the check `use`'s member registration already applies.
    pub(super) fn check_namespaced_visibility(
        &mut self,
        target: &ModuleSource,
        name: &str,
        span: crate::token::Span,
    ) {
        let Some(visibility) =
            self.symbols
                .visibility_barrier(&self.current_module_source, target, name)
        else {
            return;
        };
        let _ = self.emit(types::TypeError::PrivateNamespacedSymbol {
            name: name.to_string(),
            module_source: target.clone(),
            visibility,
            span,
        });
    }

    /// The type a transparent alias or newtype stands for — `ByteList` for
    /// `pub type ByteList = List<u8>;` — since the alias declares no method of
    /// its own. `None` when `name` names no such declaration.
    pub(super) fn newtype_base(&self, name: &str) -> Option<(tir::TypeId, String)> {
        // `type Buf = ByteList;` chains, so this peels to the type that
        // declares methods rather than stopping at the first link.
        let mut current = self.lookup_newtype(name)?;
        loop {
            let peeled = match self.tysys.type_table.borrow().get(current).clone() {
                tir::ResolvedType::Newtype { base_type, .. } => base_type,
                tir::ResolvedType::Flags { .. } => tir::TypeTable::U32,
                _ => break,
            };
            if peeled == current {
                break;
            }
            current = peeled;
        }
        Some((current, self.tysys.get_ultimate_base_struct_name(current)))
    }

    /// The declaration a `Type::method` call resolves to, seeing through a
    /// transparent alias. Every site that records the use→def edge for such a
    /// call ends its ladder here, so none of them stops one alias short.
    pub(super) fn static_method_decl_at(
        &self,
        site: Option<crate::ast::AstId>,
        type_name: &str,
        method_name: &str,
    ) -> Option<crate::defs::DefId> {
        self.static_method_decl_id(&self.impl_target_at(site, type_name), method_name)
            .or_else(|| {
                let (base, base_name) = self.newtype_base(type_name)?;
                let receiver = self.impl_target_of(base, &crate::name::DeclName::new(&base_name));
                self.static_method_decl_id(&receiver, method_name)
            })
    }

    /// The declaring node of the static method `receiver::method_name`, from
    /// the static-method index.
    ///
    /// The receiver is a key the caller resolved from its own reference site.
    /// A second key from another vantage makes the order a silent tiebreak.
    pub(super) fn static_method_decl_id(
        &self,
        receiver: &trait_env::ImplTargetKey,
        method_name: &str,
    ) -> Option<crate::defs::DefId> {
        self.tysys
            .trait_env
            .static_method_index
            .get(receiver)?
            .iter()
            .find(|e| e.name == method_name)
            .map(|e| e.method_id)
    }

    /// Build a [`control_flow::CtrlFlowCtx`] over the currently-active
    /// module's `expression_types` map. Used by the AST-level
    /// missing-return / definite-exit walks, which answer from the recorded
    /// types rather than from a `TirBlock`.
    fn ctrl_flow_ctx(&self) -> control_flow::CtrlFlowCtx<'_> {
        control_flow::CtrlFlowCtx {
            expression_types: &self.sem.types.expression_types,
            type_table: &self.tysys.type_table,
        }
    }

    pub(super) fn ast_find_return_type_in_block(
        &self,
        block: &crate::ast::Block,
    ) -> Option<TypeId> {
        control_flow::find_return_type_in_block(self.ctrl_flow_ctx(), block)
    }

    pub(super) fn ast_block_always_exits(&self, block: &crate::ast::Block) -> bool {
        control_flow::block_always_exits(self.ctrl_flow_ctx(), block)
    }

    /// Result type of an AST block, read from `expression_types` rather
    /// than from a built `TirBlock`: types `{ … }`, `if` / `match` arms, and
    /// loop and handler bodies with no TIR in hand.
    pub(super) fn ast_block_result_type(&self, block: &crate::ast::Block) -> TypeId {
        control_flow::block_result_type(self.ctrl_flow_ctx(), block)
    }

    /// Emit `MissingReturn` when a declared non-Unit return type cannot be
    /// satisfied. Skipped for `Unit` / `Never`, for a bodyless external, and for a
    /// body whose every control path provably exits first. The analysis is
    /// definite-exit, not "contains a `return` somewhere": accepting one carried
    /// by a single `if` branch produced an invalid core Wasm module.
    pub(super) fn validate_missing_return_ast(
        &self,
        return_type: TypeId,
        body: Option<&crate::ast::Block>,
        span: crate::token::Span,
    ) {
        if return_type == crate::tir::TypeTable::UNIT || return_type == crate::tir::TypeTable::NEVER
        {
            return;
        }
        let Some(body) = body else {
            return;
        };
        if control_flow::block_always_exits(self.ctrl_flow_ctx(), body) {
            return;
        }
        let _ = self.logger.error(types::TypeError::MissingReturn {
            return_type: self.tysys.type_table.borrow().type_name(return_type),
            span,
        });
    }

    /// Record the resolved [`TypeId`] for the expression at `ast_id` — the
    /// annotation reify reads to set `TirExpr::type_id` without re-inferring.
    /// Skipped for [`TypeTable::ERROR`] and for a type still mentioning
    /// [`TypeTable::UNKNOWN`]: both mark a result the body walk will revisit, and
    /// recording the sentinel would leave an entry reify cannot consume.
    pub(super) fn record_expression_type(&mut self, ast_id: crate::ast::AstId, type_id: TypeId) {
        if type_id == TypeTable::ERROR {
            return;
        }
        // An indefinite type IS recorded, because the AST-level
        // block-result-type analysis needs to see an unresolved-null branch to
        // type the block it sits in. Readers that want a *definite*
        // type call `is_indefinite` explicitly: reify's `ann_expression_types`
        // (so a null still falls back to its `expected_type`) and the
        // missing-return walk in `control_flow.rs`.
        self.sem.types.expression_types.insert(ast_id, type_id);
    }

    /// Record a method-dispatch decision, centralised here rather than at the AST
    /// wrapper so every path through [`Self::build_tir_method_call`] leaves one
    /// uniform entry. A synthetic call passes `ast_id == None` and records
    /// nothing. Reify feeds `is_ref_impl` and `self_kind` to
    /// `adjust_receiver_for_self_kind` instead of re-running impl lookup.
    pub(super) fn record_method_dispatch(
        &mut self,
        ast_id: Option<crate::ast::AstId>,
        method_def: Option<crate::defs::DefId>,
        function_ref: &tir::FunctionRef,
        self_kind: ast::SelfKind,
        is_ref_impl: bool,
        param_is_mut: Vec<bool>,
        param_names: Vec<String>,
        param_defaults: Vec<Option<ast::Expr>>,
        return_type: TypeId,
        method_type_args: Vec<TypeId>,
        consumes_self: bool,
    ) {
        let Some(ast_id) = ast_id else { return };
        let key = ast_id;
        self.sem.types.method_dispatch.insert(
            key,
            sem::types::MethodDispatch {
                method_def,
                function_ref: function_ref.clone(),
                self_kind,
                is_ref_impl,
                param_is_mut,
                param_names,
                param_defaults,
                return_type,
                method_type_args,
                consumes_self,
            },
        );
    }

    /// Record a generic-instantiation decision for the call / struct
    /// literal / variant-ctor at `ast_id`. `type_args`
    /// is the inferred (or explicitly written) concrete type for each
    /// generic parameter in declaration order; `instance_type` is the
    /// `TypeId` of the resulting `GenericInstance` / monomorphic target.
    ///
    /// Skipped when `type_args` is empty (the site is non-generic) so the
    /// map only carries decisions reify needs.
    pub(super) fn record_generic_instantiation(
        &mut self,
        ast_id: crate::ast::AstId,
        type_args: Vec<TypeId>,
        instance_type: TypeId,
    ) {
        self.record_generic_instantiation_with_mangle(ast_id, type_args, instance_type, None);
    }

    /// Variant of [`Self::record_generic_instantiation`] that also stores the
    /// mangled name reify needs to emit on the TIR node. Used by struct
    /// literal / call sites that compute the mangled form anyway; everything
    /// else takes the default `None` through the bare helper above.
    pub(super) fn record_generic_instantiation_with_mangle(
        &mut self,
        ast_id: crate::ast::AstId,
        type_args: Vec<TypeId>,
        instance_type: TypeId,
        mangled_name: Option<String>,
    ) {
        if type_args.is_empty() && mangled_name.is_none() {
            return;
        }
        let key = ast_id;
        self.sem.types.generic_instantiations.insert(
            key,
            sem::types::GenericInstantiation {
                type_args,
                instance_type,
                mangled_name,
                is_union: false,
            },
        );
    }

    /// Record the resolved (type-arg-substituted) parameter types for the
    /// call at `ast_id`, so reify can replay per-argument expected types.
    pub(super) fn record_call_param_types(
        &mut self,
        ast_id: crate::ast::AstId,
        param_types: Vec<TypeId>,
    ) {
        let key = ast_id;
        self.sem.types.call_param_types.insert(key, param_types);
    }

    /// Record the capture-analysis result for the closure expression at
    /// `ast_id`. See [`sem::types::ClosureCaptureInfo`].
    pub(super) fn record_closure_captures(
        &mut self,
        ast_id: crate::ast::AstId,
        info: sem::types::ClosureCaptureInfo,
    ) {
        let key = ast_id;
        self.sem.types.closure_captures.insert(key, info);
    }

    /// Record the power-assert capture-slot table for the assert
    /// statement at `ast_id`. See [`sem::types::AssertCaptureInfo`].
    pub(super) fn record_assert_captures(
        &mut self,
        ast_id: crate::ast::AstId,
        info: sem::types::AssertCaptureInfo,
    ) {
        let key = ast_id;
        self.sem.types.assert_captures.insert(key, info);
    }

    /// Record the iterator-path dispatch decision for the for-of
    /// statement at `ast_id`. Tuple / variadic paths
    /// are tagged via [`sem::types::DesugarKind`] alone and leave no
    /// entry here. See [`sem::types::ForOfIteratorInfo`].
    pub(super) fn record_for_of_iterator(
        &mut self,
        ast_id: crate::ast::AstId,
        info: sem::types::ForOfIteratorInfo,
    ) {
        let key = ast_id;
        self.sem.types.for_of_iterator.insert(key, info);
    }

    /// Record the operator-dispatch decision for a binary / index
    /// expression that the elaborator lowered to a trait method call
    /// Absence of a recorded entry signals to reify that the native
    /// [`tir::TirExprKind::Binary`] / [`tir::TirExprKind::Index`] path
    /// was taken instead. See [`sem::types::OperatorDispatch`].
    pub(super) fn record_operator_dispatch(
        &mut self,
        ast_id: crate::ast::AstId,
        info: sem::types::OperatorDispatch,
    ) {
        let key = ast_id;
        self.sem.types.operator_dispatch.insert(key, info);
    }

    /// Record the resolved `IndexAssign` trait dispatch keyed by the
    /// inner `IndexExpr`'s `AstId`. See
    /// [`sem::types::TypeAnnotations::index_assign_dispatch`]. Reify
    /// reads this to emit `receiver.index_assign(idx, value)` for
    /// `arr[i] = v` and `arr[i] OP= v` shapes — separate from the
    /// read-side `operator_dispatch` that carries the `IndexValue` /
    /// `Index` dispatch keyed by the same `AstId`.
    pub(super) fn record_index_assign_dispatch(
        &mut self,
        ast_id: crate::ast::AstId,
        info: sem::types::OperatorDispatch,
    ) {
        let key = ast_id;
        self.sem.types.index_assign_dispatch.insert(key, info);
    }

    /// Record the handler-binding resolution facts keyed by the
    /// [`crate::ast::EffectHandlerBinding`]'s [`AstId`]. Reify
    /// reads this entry to enumerate the `TirHandlerBinding`s
    /// without re-running `collect_effect_impls_for_type` or the
    /// explicit-form `trait_env` validation. See
    /// [`sem::types::HandlerBindingFacts`].
    pub(super) fn record_handler_binding_facts(
        &mut self,
        ast_id: crate::ast::AstId,
        info: sem::types::HandlerBindingFacts,
    ) {
        let key = ast_id;
        self.sem.types.handler_bindings.insert(key, info);
    }

    /// The fq name of an impl receiver written as `written`.
    ///
    /// An fq name names its subject by the module that declares it, so a
    /// declared type is qualified through the same canonical key the impl
    /// index uses. A type parameter is a template's own binder and a
    /// builtin shape has no declaring module in any mangle, so both stay bare
    /// — matching what `TypeTable::mangle_type_arg_for_generic` produces for
    /// the same type on the consuming side.
    pub(super) fn qualified_receiver_name(&self, written: &str) -> crate::name::FqTypeName {
        if self
            .annotate_ctx
            .trait_ctx
            .type_params
            .contains_key(written)
        {
            return crate::name::FqTypeName::binder(written);
        }
        if crate::name::is_builtin_shape_name(written) {
            return crate::name::FqTypeName::builtin(written);
        }
        self.decl_key_or_local(written).map_or_else(
            // A name that reaches no declaration at all: it names a shape or
            // nothing, and the writing module is the only vantage left.
            || crate::name::FqTypeName::shape(&self.current_module_source, written),
            |def| crate::name::FqTypeName::of_head(self.tysys.resolutions.defs(), def),
        )
    }

    /// The spelling `def` renders to in a mangled head — its declared name,
    /// with a function-local declaration's disambiguator applied.
    pub(super) fn decl_render_name(&self, def: crate::defs::DefId) -> String {
        trait_env::render_decl_name(self.tysys.resolutions.defs(), def)
    }

    /// The declaration `ns::Name` reaches from this module.
    ///
    /// A namespace import enters its module's declarations under `ns$Name`
    /// aliases, so this is the import tier answering the qualification the
    /// programmer wrote — the same answer the resolve walk gives `ns::Name` in
    /// type position, rather than a second lookup beside it.
    pub(super) fn namespace_member(
        &self,
        namespace: &str,
        name: &str,
    ) -> Option<crate::defs::DefId> {
        self.tysys.resolutions.imported_as(
            &self.current_module_source,
            &crate::name::namespace_member_alias(namespace, name),
        )
    }

    /// The declared name of the trait `trait_name` refers to here — the same
    /// name past a `use … as` alias.
    pub(super) fn declared_trait_name(&self, trait_name: &str) -> String {
        self.decl_key_or_local(trait_name).map_or_else(
            || trait_name.to_string(),
            |def| self.tysys.resolutions.defs().name(def).to_string(),
        )
    }

    /// The declaration a written reference names, keyed on the site that wrote it,
    /// so an alias, a namespace prefix and a function-local item each reach their
    /// own. `name` is read only where the site reaches nothing.
    pub(crate) fn decl_key_at(
        &self,
        site: crate::ast::AstId,
        name: &str,
    ) -> Option<crate::defs::DefId> {
        self.tysys
            .resolutions
            .declared_if_walked(site)
            .or_else(|| self.decl_key_or_local(name))
    }

    /// The declaration indexes, for a caller holding a spelling whose reference
    /// site is not at hand — a rendered head, a synthesis target. Each frame is
    /// one module, so a hit is unique; an unaccounted name falls to the prelude.
    ///
    /// The frames are the walk's own position, never a caller's. Where it reads an
    /// expression another module wrote, that writing module answers first.
    pub(crate) fn decl_key_or_local(&self, name: &str) -> Option<crate::defs::DefId> {
        // A binder shadows every declaration of its name and has no identity of
        // its own; the indexes cannot see binders and would answer `struct T`.
        if self.annotate_ctx.trait_ctx.type_params.contains_key(name) {
            return None;
        }
        let defs = self.tysys.resolutions.defs();
        let frames = self
            .annotate_ctx
            .default_scope_module
            .iter()
            .chain(std::iter::once(&self.current_module_source));
        for frame in frames {
            let found = self.tysys.resolutions.imported_as(frame, name).or_else(|| {
                self.tysys
                    .trait_env
                    .decls_named(name)
                    .find(|def| defs.module(*def) == frame)
            });
            if found.is_some() {
                return found;
            }
        }
        self.tysys.resolutions.prelude_decl(name)
    }

    /// The trait a bound's reference site names; `written` supplies the type
    /// arguments and the diagnostic spelling.
    ///
    /// A site naming no declaration gets no invented identity — `use` and the
    /// prelude are the only ways to name a trait, so a name reaching nothing here
    /// reaches nothing at all, and the mangle falls back to the spelling.
    pub(super) fn fq_trait_name_at(
        &self,
        site: crate::ast::AstId,
        written: &str,
    ) -> crate::name::FqTraitName {
        let resolutions = &self.tysys.resolutions;
        let answer = resolutions.get(site);
        if let crate::resolve::Resolution::Binder(_) = answer {
            return crate::name::FqTraitName::binder(written);
        }
        // `written` is a bound's spelling, and a bound is a bare name: the
        // parser reads `<...>` after one as associated-type bindings, so no
        // type argument ever reaches here to be split back out.
        resolutions.declared(site).map_or_else(
            || crate::name::FqTraitName::binder(written),
            |def| crate::name::FqTraitName::declared(resolutions.defs(), def),
        )
    }

    /// The trait a reference site names, in the form a mangled method name
    /// embeds it: the declaration the site resolves to, plus the type
    /// arguments the site wrote.
    ///
    /// The answer comes from [`crate::resolve::Resolutions`] — resolved once,
    /// in the module that wrote the reference — so an alias and a second
    /// module's same-named trait cannot reach the mangle. A site that names no
    /// declaration carries no identity — see [`Self::fq_trait_name_at`].
    pub(super) fn fq_trait_name(&self, ty: &ast::Type) -> crate::name::FqTraitName {
        let written = self.get_type_name(ty);
        let args = trait_env::written_type_args(ty, &self.tysys.resolutions);
        let head = crate::resolve::head_site(ty)
            .and_then(|site| {
                let resolutions = &self.tysys.resolutions;
                match resolutions.get(site) {
                    crate::resolve::Resolution::Binder(_) => {
                        Some(crate::name::FqTraitName::binder(&written))
                    }
                    _ => resolutions
                        .declared(site)
                        .map(|def| crate::name::FqTraitName::declared(resolutions.defs(), def)),
                }
            })
            .unwrap_or_else(|| crate::name::FqTraitName::binder(&written));
        head.with_args(args)
    }

    /// Record the impl-block resolution facts keyed by the
    /// [`crate::ast::ImplBlock`]'s [`AstId`]. Reify
    /// reads the entry verbatim — no re-resolution of the impl
    /// target, the trait reference, the type params, or the
    /// associated types happens inside `reify_impl`.
    /// See [`sem::types::ImplFacts`].
    pub(super) fn record_impl_facts(
        &mut self,
        ast_id: crate::ast::AstId,
        info: sem::types::ImplFacts,
    ) {
        let key = ast_id;
        self.sem.types.impl_facts.insert(key, info);
    }

    /// Record an `impl Trait for Type;` synthesis request. `reify_module` reads
    /// [`sem::decls::ModuleDecls::pending_synthesis_requests`] and
    /// pushes each onto the emitted [`tir::TirModule::synthesis_requests`].
    pub(super) fn record_pending_synthesis_request(&mut self, req: tir::SynthesisRequest) {
        self.sem.decls.pending_synthesis_requests.push(req);
    }

    fn classify_from_marker(&mut self, trait_type: &ast::Type) -> Option<tir::SynthTrait> {
        use crate::compiler_item::CompilerItem;
        let base = trait_type.head_base_name()?;
        {
            let tt = self.tysys.type_table.borrow();
            if tt.compiler_items().trait_name_opt(CompilerItem::From) != Some(base) {
                return None;
            }
        }
        if let ast::Type::Generic(generic) = trait_type
            && generic.args.len() == 1
        {
            let source = self.resolve_type(&generic.args[0]);
            Some(tir::SynthTrait::From { source })
        } else {
            None
        }
    }

    fn classify_on_bound_marker(&self, trait_type: &ast::Type) -> Option<String> {
        let base = trait_type.head_base_name()?;
        self.tysys
            .classify_on_bound_trait(&self.type_lookup(), base)
            .map(|_| base.to_string())
    }

    fn record_explicit_derive_request(
        &mut self,
        trait_type: &ast::Type,
        trait_name: &str,
        target_type_id: TypeId,
        target_type_name: &str,
        span: crate::token::Span,
    ) {
        if target_type_id == tir::TypeTable::ERROR {
            return;
        }
        if self.tysys.structurally_derivable_for_explicit_request(
            &self.annotate_ctx,
            &self.type_lookup(),
            target_type_id,
            trait_name,
        ) {
            let module_source = self
                .tysys
                .type_table
                .borrow()
                .nominal_head(target_type_id)
                .map(|(_, m)| m)
                .unwrap_or_else(|| {
                    unreachable!("explicit derive marker validated for a non-nominal type")
                });
            // The marker's own site says which trait it names; the request is
            // keyed by that declaration, not by the spelling.
            if let Some(key) = self.fq_trait_name(trait_type).canonical() {
                self.tysys
                    .type_table
                    .borrow_mut()
                    .record_bound_driven_synth_request_for(target_type_id, &module_source, &key);
            }
            return;
        }
        let receiver = Receiver::Type(self.tysys.fq_receiver_head(target_type_id));
        // The marker's own site says which trait it names, so an already-present
        // impl is recognised by declaration rather than by a spelling another
        // module's trait can share.
        let requested = crate::resolve::head_site(trait_type)
            .and_then(|site| self.tysys.resolutions.declared(site));
        if requested.is_some_and(|trait_| {
            self.tysys.has_real_trait_impl_for_type(
                &self.annotate_ctx,
                &self.type_lookup(),
                Some(target_type_id),
                &receiver,
                trait_,
            )
        }) {
            return;
        }
        let reason = self.tysys.trait_unimpl_reason_chain(
            &self.annotate_ctx,
            &self.type_lookup(),
            target_type_id,
            trait_name,
        );
        let _ = self
            .logger
            .error(types::TypeError::ExplicitDeriveNotEligible {
                trait_name: self.get_type_name_full(trait_type),
                type_name: target_type_name.to_string(),
                reason,
                span,
            });
    }

    fn resolve_synthesize_request_marker(
        &mut self,
        impl_block: &ast::ImplBlock,
        struct_name: &str,
    ) {
        let Some(trait_type) = &impl_block.trait_type else {
            return;
        };
        if let Some(trait_name) = self.classify_on_bound_marker(trait_type) {
            let target_type_id = self.resolve_type(&impl_block.ty);
            self.record_explicit_derive_request(
                trait_type,
                &trait_name,
                target_type_id,
                struct_name,
                impl_block.span,
            );
        } else if let Some(trait_ref) = self.classify_from_marker(trait_type) {
            let target_type_id = self.resolve_type(&impl_block.ty);
            let type_params: Vec<_> = self
                .annotate_ctx
                .trait_ctx
                .type_params
                .iter()
                .map(|(name, &(index, type_id))| (name.clone(), index, type_id))
                .collect();
            self.record_pending_synthesis_request(tir::SynthesisRequest {
                trait_ref,
                target_type_name: struct_name.to_string(),
                target_type_id,
                type_params,
                span: impl_block.span,
            });
        } else {
            let is_display = trait_type
                .head_base_name()
                .is_some_and(|base| self.tysys.is_display_trait(&self.type_lookup(), base));
            let _ = self
                .logger
                .error(types::TypeError::UnsupportedSynthesisTrait {
                    trait_name: self.get_type_name_full(trait_type),
                    type_name: struct_name.to_string(),
                    is_display,
                    span: impl_block.span,
                });
        }
    }

    /// Record a coercion decision for the expression at `ast_id`. Called
    /// from each successful `try_coerce_*` sub-helper so every caller of
    /// those helpers (`try_coerce`, `resolve_cast`, the deferred-coercion
    /// fixup in struct-literal resolution, and `resolve_let`'s
    /// struct-to-map path) records the choice at the decision point — no
    /// branch can bypass it.
    pub(super) fn record_coercion(
        &mut self,
        ast_id: crate::ast::AstId,
        kind: sem::types::CoercionKind,
        target_type: TypeId,
    ) {
        let key = ast_id;
        self.sem
            .types
            .coercions
            .insert(key, sem::types::CoercionChoice { kind, target_type });
    }

    /// Record the assignment-target place classification for the identifier
    /// at `ast_id`. Called from [`Self::resolve_ident`] at each
    /// site that resolves to a place (local / `&mut`-deref-capture / global)
    /// so [`Self::assign_to_target`] can validate l-values and global
    /// mutability from the AST + this fact instead of the now-placeholder
    /// resolved `target.kind`. See [`sem::types::AssignPlace`].
    pub(super) fn record_assign_place(
        &mut self,
        ast_id: crate::ast::AstId,
        place: sem::types::AssignPlace,
    ) {
        let key = ast_id;
        self.sem.types.assign_places.insert(key, place);
    }

    /// Look up the recorded assignment-target place classification for the
    /// identifier at `ast_id`. Returns `None` for idents that did not resolve
    /// to a place (functions, variants, enums, flags, constants).
    pub(super) fn assign_place_of(
        &self,
        ast_id: crate::ast::AstId,
    ) -> Option<&sem::types::AssignPlace> {
        self.sem.types.assign_places.get(&ast_id)
    }

    /// Record a TIR-direct desugar tag for the AST node at `ast_id`.
    /// Called from each `assert` / `matches` / comparison-chain / for-of
    /// / `while` / compound-assignment lowering site so the future
    /// `reify` pass can pick the same expansion path. See
    /// [`sem::types::DesugarKind`].
    pub(super) fn record_desugar(
        &mut self,
        ast_id: crate::ast::AstId,
        kind: sem::types::DesugarKind,
    ) {
        let key = ast_id;
        self.sem.types.desugars.insert(key, kind);
    }

    /// Record a local binding's [`Symbol`] and resolved [`TypeId`] so that
    /// LSP hover on a use site can retrieve the defining name / mutability
    /// and inlay hints can surface the inferred type. Called at each site
    /// where a user-visible local is introduced.
    pub(super) fn record_local_symbol(
        &mut self,
        def_id: crate::ast::AstId,
        name: &str,
        span: crate::token::Span,
        is_mut: bool,
        type_id: TypeId,
    ) {
        let symbol = Symbol {
            name: name.to_string(),
            kind: crate::symbol::SymbolKind::Variable(crate::symbol::VariableSymbol {
                is_mut,
                is_reactive: false,
            }),
            defined_at: def_id,
            module: self.current_module_source.clone(),
            visibility: crate::ast::Visibility::Private,
            span: Some(span),
        };
        self.sem.bindings.local_symbols.insert(def_id, symbol);
        self.sem.types.local_types.insert(def_id, type_id);
    }

    /// The impl-associated constant `owner` declares as `name`.
    ///
    /// `owner` is the declaration the use site's qualifier resolved to — an
    /// alias and a `ns$Type` prefix answer with it like any other spelling —
    /// so a same-named type in an unrelated module can never satisfy the
    /// lookup.
    pub(super) fn associated_constant_of(
        &self,
        owner: crate::defs::DefId,
        name: &str,
    ) -> Option<sig::AssocConstSig> {
        self.tysys
            .signatures
            .associated_constant(owner, name)
            .cloned()
    }

    /// [`Self::associated_constant_of`] for a qualified path in expression
    /// position, whose leading segment carries the site that names the owner.
    pub(super) fn associated_constant_of_path(
        &self,
        ident: &ast::IdentExpr,
    ) -> Option<sig::AssocConstSig> {
        let owner = trait_query::assoc_const_owner_of_path(ident, &self.tysys.resolutions)?;
        let name = ident.segments.last()?;
        self.associated_constant_of(owner, &name.name)
    }

    /// [`Self::associated_constant_of`] for a pattern's `Type::CONST`
    /// spelling, whose qualifier is a written `ast::Type` with its own site.
    pub(super) fn associated_constant_qualified(
        &self,
        qualifier: Option<&ast::Type>,
        name: &str,
    ) -> Option<sig::AssocConstSig> {
        let owner = trait_query::assoc_const_owner(qualifier, &self.tysys.resolutions)?;
        self.associated_constant_of(owner, name)
    }

    /// The declaration a qualified path's *owner* segment names — `Color` in
    /// `Color::Red`, `Color` in `ns::Color::Red` — read off the site the
    /// resolve walk answered for. `None` for a bare name, which qualifies
    /// nothing, and for an owner that reaches no declaration.
    pub(crate) fn qualified_owner_decl(
        &self,
        ident: &ast::IdentExpr,
    ) -> Option<crate::defs::DefId> {
        let owner = ident.segments.len().checked_sub(2)?;
        self.tysys.resolutions.declared(ident.segments[owner].id)
    }

    /// Field info for the declaration a *written* struct name resolved to.
    ///
    /// `None` where the name reached nothing, or reached something that is no
    /// struct — the caller has already diagnosed it and is carrying on.
    pub(super) fn struct_fields_of_written_decl(
        &self,
        decl: Option<crate::defs::DefId>,
    ) -> Option<&StructFieldInfo> {
        self.lookup_struct_fields_of_decl(decl?)
    }

    /// Field info for the struct `def` declares.
    pub(super) fn lookup_struct_fields_of_decl(
        &self,
        def: crate::defs::DefId,
    ) -> Option<&StructFieldInfo> {
        self.type_lookup().struct_fields_of(def)
    }

    /// Field info for a struct type's head; see
    /// [`TypeLookup::struct_fields_of_head`].
    pub(super) fn lookup_struct_fields_of(
        &self,
        head: crate::tir::StructDef,
    ) -> Option<&StructFieldInfo> {
        self.type_lookup().struct_fields_of_head(head)
    }

    /// The variant `type_id` is an instance of, or `None` when it is not one.
    ///
    /// Asks the type for its declaration instead of reading a `(name, module)`
    /// pair off it: an instantiated `Option<i32>` answers with the `Option` it
    /// was spelled from, so there is no separate generic arm and no spelling
    /// check deciding whether the pair means a variant at all.
    pub(super) fn variant_of_type(&self, type_id: TypeId) -> Option<&VariantInfo> {
        let def = self.tysys.type_def(type_id)?;
        self.tysys.all_variant_cases.get(&def)
    }

    /// The enum `type_id` is, or `None` when it is not one. Asks the type for
    /// its declaration, the way [`Self::variant_of_type`] does.
    pub(super) fn enum_of_type(&self, type_id: TypeId) -> Option<&EnumInfo> {
        let def = self.tysys.type_def(type_id)?;
        self.tysys.all_enum_cases.get(&def)
    }

    /// The struct `type_id` is an instance of; see [`Self::variant_of_type`].
    pub(super) fn struct_fields_of_type(&self, type_id: TypeId) -> Option<&StructFieldInfo> {
        let def = self.tysys.type_def(type_id)?;
        self.lookup_struct_fields_of_decl(def)
    }

    /// Build effect name → declaring module map for a module.
    ///
    /// Three sources, applied in *increasing* precedence — each overwrites the
    /// last, so the one written last here is the one that answers:
    ///
    /// 1. `use { Iface::{f} }`, which names an interface without importing it,
    ///    so the `use` declaration is the only record of what `Iface` means;
    /// 2. the module's explicit imports, read from the symbol table where the
    ///    analyzer already resolved aliases and re-export chains;
    /// 3. the module's own `interface` / `resource` declarations, which win
    ///    over any import of the name.
    ///
    /// The order is what the third clause requires, and stating it as a list
    /// of decreasing precedence read the other way round.
    ///
    /// An import earns an entry by *being* an effect or a resource, asked of
    /// the declaration. Guessing from the spelling — the `PascalCase` test this
    /// replaces — admitted every imported struct and let a same-named one
    /// answer for an effect it has nothing to do with.
    fn build_effect_sources(
        interner: &mut ModuleSourceInterner,
        module: &Module,
        module_source: &ModuleSource,
        entry: Option<&ModuleSource>,
        invocations: &crate::kiln::InvocationIndex,
        symbols: &crate::symbol::SymbolTable,
    ) -> IndexMap<String, ModuleSource> {
        let mut sources = IndexMap::default();
        for item in &module.items {
            let Item::Use(use_decl) = item else {
                continue;
            };
            let mut interfaces = use_decl.items.iter().filter_map(|use_item| match use_item {
                ast::UseItem::InterfaceFunctions { interface_name, .. } => Some(interface_name),
                ast::UseItem::Simple { .. }
                | ast::UseItem::Wildcard
                | ast::UseItem::Namespace { .. } => None,
            });
            if let Some(first) = interfaces.next() {
                // `entry` must be threaded so identities match the loader
                // (see `name::resolve_local_identity`). Wasm-asset imports
                // resolve to `ModuleSource::Wasm`, matching the loader.
                let source = crate::loader::resolve_use_decl_source(
                    interner,
                    module_source,
                    use_decl,
                    entry,
                    invocations,
                );
                for interface_name in std::iter::once(first).chain(interfaces) {
                    sources.insert(interface_name.clone(), source.clone());
                }
            }
        }
        for (local_name, sym) in symbols.imports_in(module_source) {
            if matches!(
                sym.kind,
                crate::symbol::SymbolKind::Effect(_) | crate::symbol::SymbolKind::Resource(_)
            ) {
                sources.insert(local_name.to_string(), sym.module_source().clone());
            }
        }
        for item in &module.items {
            match item {
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

    /// Canonical impl-target key for a type named at a use site, for the impl
    /// indexes.
    ///
    /// Through [`Self::decl_key_or_local`], so an impl target and every other
    /// name-only lookup answer from one chain. The table alone leaves out the
    /// declaration indexes, and a receiver the module never imported — reached
    /// only through a return type — would then key to the call site instead of
    /// where it is declared.
    pub(crate) fn impl_target(&self, type_name: &str) -> trait_env::ImplTargetKey {
        self.impl_target_at(None, type_name)
    }

    /// [`Self::impl_target`] for a receiver written at a reference site, so
    /// `Type::method` keys to what `Type` names *in the module that wrote it* —
    /// a default spliced into a same-named caller is where the two come apart.
    ///
    /// A binder answers nothing, so `Self::` / `T::` falls through to the
    /// spelling, by then the concrete name the rewrite produced.
    pub(crate) fn impl_target_at(
        &self,
        site: Option<crate::ast::AstId>,
        type_name: &str,
    ) -> trait_env::ImplTargetKey {
        let defs = self.tysys.resolutions.defs();
        site.and_then(|site| self.tysys.resolutions.declared_if_walked(site))
            .or_else(|| self.decl_key_or_local(type_name))
            .map_or_else(
                || trait_env::ImplTargetKey::of_undeclared(&self.current_module_source, type_name),
                |def| trait_env::ImplTargetKey::of_decl(defs, def),
            )
    }

    /// Impl-target key for a receiver whose `TypeId` is known. The type
    /// already carries its declaring module, so this asks it rather than
    /// re-resolving the name from the caller's vantage — which cannot
    /// separate two modules' same-named types when the caller imports
    /// neither, and reaches the receiver only through a return type.
    pub(crate) fn impl_target_of(
        &self,
        type_id: tir::TypeId,
        fallback_name: &crate::name::DeclName,
    ) -> trait_env::ImplTargetKey {
        match self.type_decl_key(type_id) {
            Some(def) => trait_env::ImplTargetKey::of_decl(self.tysys.resolutions.defs(), def),
            None => self.impl_target(fallback_name.as_decl_str()),
        }
    }

    /// Name of a type whose identity is the name itself — a primitive, `()`,
    /// `!` or the raw `Array<T>`.
    fn builtin_type_name(&self, type_id: tir::TypeId) -> Option<String> {
        use crate::tir::ResolvedType;
        let tt = self.tysys.type_table.borrow();
        match tt.get(tt.peel_refs(type_id)) {
            ResolvedType::Primitive(prim) => Some(prim.as_str().to_string()),
            ResolvedType::Unit => Some(tir::TypeTable::UNIT_TYPE_NAME.to_string()),
            ResolvedType::Never => Some("!".to_string()),
            ResolvedType::BuiltinArray(_) => Some(tir::TypeTable::ARRAY_TYPE_NAME.to_string()),
            _ => None,
        }
    }

    /// The declaration behind `type_id` (refs peeled), or `None` for a type
    /// parameter, an associated-type projection, an anonymous struct shape and
    /// the other shapes that name none.
    ///
    /// For a generic instance it is the *base* type's declaration — type
    /// arguments are dropped, so it cannot tell `Foo<A>` from `Foo<B>`.
    pub(crate) fn type_decl_key(&self, type_id: tir::TypeId) -> Option<crate::defs::DefId> {
        // A builtin's identity is its name, and the name path already knows
        // which module declares it. A second table answering here would be a
        // second derivation, free to disagree with that one.
        if let Some(name) = self.builtin_type_name(type_id) {
            return self.decl_key_or_local(&name);
        }
        let tt = self.tysys.type_table.borrow();
        tt.nominal_def(tt.peel_refs(type_id))
    }

    /// Decl identity of the link in `type_id`'s newtype chain that `impl_name`
    /// declares — the head when an impl is written on the newtype itself, a
    /// base when the newtype inherited it. `None` when no link carries that
    /// name, leaving the caller to fall back to a by-name lookup.
    pub(crate) fn impl_target_decl_key(
        &self,
        type_id: tir::TypeId,
        impl_name: &str,
    ) -> Option<crate::defs::DefId> {
        use crate::tir::ResolvedType;
        let mut current = self.tysys.type_table.borrow().peel_refs(type_id);
        loop {
            // `impl_name` is a receiver name, so it carries its declaring
            // module. Build the same form from the candidate declaration
            // rather than taking `impl_name` apart — a name is assembled,
            // never parsed.
            if let Some(key) = self.type_decl_key(current) {
                let defs = self.tysys.resolutions.defs();
                if self.decl_render_name(key) == impl_name
                    || crate::name::FqTypeName::declared(defs, key).to_mangled() == impl_name
                {
                    return Some(key);
                }
            }
            current = {
                let tt = self.tysys.type_table.borrow();
                match tt.get(current) {
                    ResolvedType::Newtype { base_type, .. } => *base_type,
                    ResolvedType::Flags { .. } => tir::TypeTable::U32,
                    _ => return None,
                }
            };
        }
    }

    /// Resolve AST effect names (strings) to TIR `EffectRefs` with module source information.
    ///
    /// `effect_ids` is a parallel slice with `(AstId, Span)` of each effect-name identifier
    /// occurrence. When non-empty, use→def edges are recorded so LSP jump-to-definition
    /// works on effect references in `with` clauses. An empty slice skips recording
    /// (used by synthetic/internal effect lists with no source identifiers).
    pub(crate) fn resolve_effects(
        &mut self,
        effects: &[String],
        effect_ids: &[(crate::ast::AstId, crate::token::Span)],
    ) -> Vec<tir::EffectRef> {
        effects
            .iter()
            .enumerate()
            .map(|(i, name)| {
                let use_id = effect_ids.get(i).map(|(id, _)| *id);
                if let Some(&decl_id) = self.annotate_ctx.trait_ctx.effect_params.get(name) {
                    if let Some(use_id) = use_id {
                        self.record_reference(use_id, decl_id);
                    }
                    tir::EffectRef::Param { name: name.clone() }
                } else if let Some(def) = self.decl_key_or_local(name).filter(|def| {
                    self.tysys.trait_env.effect_decl_index.contains(def)
                        || self.tysys.trait_env.resource_decl_index.contains(def)
                }) {
                    if let Some(use_id) = use_id {
                        let decl_ast = self.tysys.resolutions.defs().ast_id(def);
                        self.record_reference_to_def(use_id, decl_ast);
                    }
                    let defs = self.tysys.resolutions.defs();
                    tir::EffectRef::Concrete {
                        name: defs.name(def).to_string(),
                        module_source: defs.module(def).clone(),
                    }
                } else if let Some(source) = self.sem.imports.effect_sources.get(name).cloned() {
                    // Identity is the declaration, so two `with Stdout` clauses
                    // — one importing from `core:cli`, one from `wasi:cli` —
                    // and a `with Out` aliasing either name one effect.
                    let declared = self
                        .symbols
                        .lookup_in_module(&source, name)
                        .map(|sym| {
                            if let Some(use_id) = use_id {
                                self.record_reference_to_def(use_id, sym.defined_at);
                            }
                            (sym.name.clone(), sym.module_source().clone())
                        })
                        .unwrap_or_else(|| (name.clone(), source.clone()));
                    tir::EffectRef::Concrete {
                        name: declared.0,
                        module_source: declared.1,
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
                    let declared = self
                        .symbol_named(&self.current_module_source, name)
                        .map(|sym| {
                            if let Some(use_id) = use_id {
                                self.record_reference_to_def(use_id, sym.defined_at);
                            }
                            (sym.name.clone(), sym.module_source().clone())
                        })
                        .unwrap_or_else(|| (name.clone(), self.current_module_source.clone()));
                    tir::EffectRef::Concrete {
                        name: declared.0,
                        module_source: declared.1,
                    }
                }
            })
            .collect()
    }

    /// Record use→def edges for each imported name in `use { a, b as c } from "..."`
    /// declarations. The cursor landing on an imported name inside a `use`
    /// specifier list should jump to the defining symbol in the source module.
    fn record_use_specifier_references(&mut self, module: &Module) {
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
                            self.record_reference_to_def(*id, sym.defined_at);
                        }
                    }
                    ast::UseItem::InterfaceFunctions { .. }
                    | ast::UseItem::Wildcard
                    | ast::UseItem::Namespace { .. } => {}
                }
            }
        }
    }

    /// Per-module declaration pass (`annotate_decls`): effect sources,
    /// use-specifier use→def edges, type / signature collection, globals,
    /// associated constants, and the generic-function inference caches.
    /// Populates `ModuleSemantics.imports` / `.decls`; walks no bodies.
    /// The driver runs this for every module before any body walk
    /// ([`Self::annotate_module_bodies`]).
    pub fn annotate_module_decls(&mut self, module: &'a Module, module_source: ModuleSource) {
        self.current_module_source = module_source.clone();
        self.sem.imports.effect_sources = Self::build_effect_sources(
            &mut self.interner.borrow_mut(),
            module,
            &module_source,
            Some(&self.entry_module_source),
            &self.invocations,
            self.symbols,
        );

        // Record use→def edges for names that appear inside `use { ... }` specifiers.
        // These power LSP jump-to-definition when the cursor is on an imported
        // name in the `use` declaration itself.
        self.record_use_specifier_references(module);

        // First pass: collect type definitions
        {
            let _span = self.logger.span("elaborate/collect_types");
            self.collect_types(module);
        }

        // Second pass: collect function signatures (for call resolution)
        {
            let _span = self.logger.span("elaborate/collect_sigs");
            self.collect_function_signatures(module);
        }

        // This module's own globals. The ones it *imports* are filled in
        // afterwards, by the driver: an imported global's type means what the
        // declaring module wrote (`global RK_PROG: NodeKind` names a newtype
        // the importer never brought into scope), and that is a declaration
        // fact — available once every module's decl pass has run, never by
        // re-resolving the declaring module's AST here.
        self.sem.decls.current_module_globals.clear();
        for item in &module.items {
            if let Item::Global(global_decl) = item {
                let ty = self.resolve_type(&global_decl.ty);
                self.sem
                    .decls
                    .current_module_globals
                    .insert(global_decl.name.clone(), (ty, global_decl.mutable));
            }
        }

        // This module's impl-associated constants, each keyed by canonical
        // identity — the impl target's prefix canonicalized here, in the
        // declaring scope — so the driver-merged view cannot collide across
        // same-named types. Lookups canonicalize the queried prefix the
        // same way ([`Self::associated_constant_of`] and its path / qualified
        // forms).
        self.sem.decls.associated_constants.clear();
        type AssocConstInput = (
            String,
            String,
            ast::Type,
            ast::Expr,
            Option<ast::Visibility>,
        );
        let assoc_const_inputs: Vec<AssocConstInput> = module
            .items
            .iter()
            .filter_map(|item| {
                if let Item::Impl(impl_block) = item {
                    Some(impl_block)
                } else {
                    None
                }
            })
            .flat_map(|impl_block| {
                let type_name = self.get_type_name(&impl_block.ty);
                let is_inherent = impl_block.trait_type.is_none();
                impl_block
                    .constants
                    .iter()
                    .map(move |assoc_const| {
                        (
                            type_name.clone(),
                            assoc_const.name.clone(),
                            assoc_const.ty.clone(),
                            assoc_const.value.clone(),
                            is_inherent.then_some(assoc_const.visibility),
                        )
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
        for (type_name, const_name, ty, value, inherent_visibility) in assoc_const_inputs {
            let type_id = self.resolve_type(&ty);
            let Some(owner) = self.decl_key_or_local(&type_name) else {
                continue;
            };
            self.sem.decls.associated_constants.insert(
                (owner, const_name),
                sig::AssocConstSig {
                    module: module_source.clone(),
                    ty: type_id,
                    value,
                    inherent_visibility,
                },
            );
        }

        // Must stay in the decl pass: `Signatures` is assembled once every
        // module's has run, so a body-pass record is invisible to every query.
        self.sem.decls.trait_sigs.clear();
        let trait_decls: Vec<ast::TraitDecl> = module
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Trait(decl) => Some(decl.clone()),
                _ => None,
            })
            .collect();
        for trait_decl in &trait_decls {
            self.resolve_trait_decl(trait_decl);
        }

        // Resolve each `interface` / `resource` declaration's operations in
        // its own frame, so a generic resource's methods see its type params
        // and `Self`. The body pass reads these back rather than repeating
        // the work.
        self.sem.decls.effect_ops.clear();
        self.sem.decls.resource_method_ids.clear();
        let decl_ops: Vec<(
            crate::ast::AstId,
            Vec<ast::GenericParam>,
            Vec<ast::Function>,
            bool,
        )> = module
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Interface(decl) => Some((decl.id, Vec::new(), decl.methods.clone(), false)),
                Item::Resource(decl) => Some((
                    decl.id,
                    decl.type_params.clone(),
                    decl.methods.clone(),
                    true,
                )),
                _ => None,
            })
            .collect();
        for (decl_id, type_params, methods, is_resource) in decl_ops {
            let resource_self = is_resource
                .then(|| self.tysys.resolutions.defs().of_ast_id(decl_id))
                .flatten();
            let ops = self.resolve_effect_ops(&type_params, &methods, resource_self);
            let defs = std::sync::Arc::clone(self.tysys.resolutions.defs());
            let owner = defs.def_at(decl_id);
            for method in &methods {
                let op = defs.def_at(method.id);
                self.sem
                    .decls
                    .resource_method_ids
                    .insert((owner, method.name.clone()), op);
            }
            self.sem.decls.effect_ops.insert(owner, ops);
        }

        // Pre-populate the generic-function inference caches for every
        // generic function in the current module. This allows same-module
        // forward references (e.g. `outer<T>` calling `inner<T>` defined
        // later in the file) to infer type arguments at the call site
        // during body resolution, without relying on a later
        // monomorphization-time fallback.
        let mut function_sigs: IndexMap<crate::defs::DefId, Rc<sem::decls::FunctionSig>> =
            IndexMap::default();
        for item in &module.items {
            if let Item::Function(func) = item {
                let def = self.def_at(func.id);
                let sig = self.record_function_sig(func);
                function_sigs.insert(def, Rc::new(sig));
            }
        }
        for item in &module.items {
            if let Item::Impl(impl_block) = item {
                self.record_impl_decls(impl_block);
            }
        }
        self.sem.decls.function_sigs = Rc::new(function_sigs);
        for item in &module.items {
            if let Item::Function(func) = item {
                self.precompute_generic_function_cache(func);
            }
        }
    }

    /// Per-module body walk (`annotate_bodies`): resolves every item body,
    /// recording types / dispatch / signatures / desugar facts and emitting
    /// diagnostics, but builds no TIR — reify (`reify_module`) is the sole
    /// `TirModule` producer, and it reads these facts. Requires
    /// [`Self::annotate_module_decls`] to have populated this module's
    /// `ModuleSemantics` first.
    pub fn annotate_module_bodies(&mut self, module: &'a Module, module_source: ModuleSource) {
        self.current_module_source = module_source;
        let _resolve_funcs_span = self.logger.span("elaborate/resolve_funcs");

        let mut test_count = 0usize;
        for item in &module.items {
            match item {
                Item::Function(func) => {
                    self.resolve_function(func);
                }
                Item::Struct(struct_decl) => {
                    self.resolve_struct(struct_decl);
                }
                Item::Impl(impl_block) => {
                    self.resolve_impl_item(impl_block);
                }
                Item::Trait(_trait_decl) => {}
                Item::Variant(variant_decl) => {
                    self.resolve_variant_decl(variant_decl);
                }
                Item::Test(test_decl) => {
                    // Reify indexes its own tests, so nothing reads this one.
                    // Pass the running count for parity and resolve the body
                    // for its facts.
                    let test_index = test_count;
                    let module_is_todo = module.has_todo();
                    if self
                        .resolve_test_decl(test_decl, test_index, module_is_todo)
                        .is_some()
                    {
                        test_count += 1;
                    }
                }
                Item::Global(global_decl) => {
                    self.resolve_global(global_decl);
                }
                // Enum / Flags / Newtype emit no body-level facts — reify
                // rebuilds their `TirEnum` / `TirFlags` / `TirNewtype` from the
                // AST + decl tables, so the body walk does nothing for them.
                Item::Enum(_) | Item::Flags(_) | Item::Newtype(_) => {}
                Item::Interface(effect_decl) => {
                    // Records `effect_ops`; reify reads them.
                    self.resolve_effect_decl(effect_decl);
                    self.reject_unsupported_operation_clauses(
                        &effect_decl.name,
                        &effect_decl.methods,
                        crate::elaborator::item::OperationOwner::Interface,
                    );
                    // An operation's default body is walked as the function
                    // reify will emit it as, so its facts land under the same
                    // `AstId` every other function's do.
                    for method in crate::elaborator::reify::default_impl_methods(effect_decl) {
                        self.resolve_function(&method);
                    }
                }
                Item::Resource(resource_decl) => {
                    self.resolve_resource_decl(resource_decl);
                    self.reject_unsupported_operation_clauses(
                        &resource_decl.name,
                        &resource_decl.methods,
                        crate::elaborator::item::OperationOwner::Resource,
                    );
                }
                // Other items will be added as needed
                _ => {}
            }
        }

        drop(_resolve_funcs_span);

        // Solve / sweep holes minted in this module; unsolved ones raise
        // "cannot infer". After every function, so all solve points have fired.
        self.finalize_infer_holes();
    }

    /// Resolve an `impl` block item: register its type-param scope, record
    /// the impl facts, resolve its methods, and synthesise trait default
    /// methods. The guard restores the parent context on every exit path,
    /// including the synthesize-request early return.
    fn resolve_impl_item(&mut self, impl_block: &ast::ImplBlock) {
        let mut scope = self.enter_inherited_type_param_scope();
        scope.annotate_ctx.trait_ctx.type_params.clear();
        scope.annotate_ctx.trait_ctx.type_param_bounds.clear();

        // Resolve impl block methods with mangled names
        let struct_name = scope.get_type_name(&impl_block.ty);
        let trait_name = impl_block.trait_type.as_ref().map(|t| {
            let fq = scope.fq_trait_name(t);
            scope.tysys.trait_env.fq_trait_named_by_impl(
                fq,
                t,
                &impl_block.ty,
                &scope.tysys.resolutions,
            )
        });

        // Register type parameters from impl block's generic type FIRST
        // e.g., impl IndexValue<i32> for Triple<T> needs T registered

        // Register explicit type params from impl<T: Bound> declarations,
        // skipping concrete types (e.g., `impl<i32, T>` — skip "i32").
        // This handles both `impl<T> Trait for Struct<T>` and
        // `impl<T: Bound> OtherTrait for T` (T is the impl type directly).
        scope.register_impl_block_params(impl_block);

        if impl_block.is_synthesize_request {
            scope.resolve_synthesize_request_marker(impl_block, &struct_name);
            return;
        }

        // Set up associated type bindings for trait implementations
        // This now works because type params (like T) are registered above
        scope.annotate_ctx.trait_ctx.assoc_type_bindings.clear();
        if impl_block.trait_type.is_some() {
            // Resolve the target type for registering associated type resolutions
            let target_type_id = scope.resolve_type(&impl_block.ty);
            // `type Output = Self;` names this impl's target. Without the
            // binding it resolved to `unknown` and registered as one.
            scope.annotate_ctx.trait_ctx.self_type = Some(target_type_id);
            let is_concrete = !scope
                .tysys
                .type_table
                .borrow()
                .contains_type_param(target_type_id);
            // The header names one instantiation whatever it binds, so the
            // arguments are resolved once rather than per associated type.
            let impl_trait_ref = trait_name
                .as_ref()
                .and_then(crate::name::FqTraitName::canonical)
                .map(|trait_key| {
                    impl_block.trait_type.as_ref().map_or_else(
                        || crate::tir::TraitRef::bare(trait_key),
                        |t| scope.impl_trait_ref(t, &impl_block.ty, trait_key),
                    )
                });

            for binding in &impl_block.associated_types {
                let type_id = scope.resolve_type(&binding.ty);
                scope
                    .annotate_ctx
                    .trait_ctx
                    .assoc_type_bindings
                    .insert(binding.name.clone(), type_id);

                // Register in TypeTable for substitution resolution
                // Only for concrete types (not generic impls like impl<T> Trait for List<T>)
                let Some(trait_ref) = impl_trait_ref.clone() else {
                    continue;
                };
                if is_concrete {
                    scope
                        .tysys
                        .type_table
                        .borrow_mut()
                        .register_assoc_type_resolution(
                            target_type_id,
                            trait_ref,
                            binding.name.clone(),
                            type_id,
                        );
                } else {
                    // For generic impls, register the definition so the monomorphizer
                    // can resolve associated types for GenericInstance types.
                    let base_decl = scope.tysys.type_table.borrow().decl_of_type(target_type_id);
                    if let Some(base_decl) = base_decl {
                        scope
                            .tysys
                            .type_table
                            .borrow_mut()
                            .register_generic_assoc_type_def(
                                base_decl,
                                trait_ref,
                                binding.name.clone(),
                                type_id,
                            );
                    }
                }
            }
        }

        // Record the impl-block
        // resolution facts so `reify_impl` can read them
        // verbatim. All inputs are already computed by
        // the setup above; the recording is one call
        // that snapshots the resolved Self type, the
        // trait canonical / mangled forms, the impl's
        // TIR type-param projection, the assoc-type
        // bindings, and the handler / ref-impl flags.
        {
            let self_type = scope.resolve_type(&impl_block.ty);
            let is_handler_method = trait_name
                .as_ref()
                .and_then(crate::name::FqTraitName::canonical)
                .is_some_and(|key| {
                    scope.tysys.trait_env.effect_decl_index.contains(&key)
                        || scope.tysys.trait_env.resource_decl_index.contains(&key)
                });
            let is_ref_impl = matches!(
                &impl_block.ty,
                ast::Type::Reference(_) | ast::Type::MutReference(_),
            );
            let qualified_struct_name = scope.qualified_receiver_name(&struct_name);
            let receiver = match RefKind::from_ast(&impl_block.ty) {
                Some(kind) => Receiver::Ref(kind),
                None => Receiver::Type(qualified_struct_name.clone()),
            };
            // Concrete type args of the impl's trait reference
            // (`impl Future<i32>` → `[i32]`), resolved in the
            // impl's type-param scope so generic impls
            // round-trip their `TypeParam` ids. Mirrors
            // `record_impl_sig`.
            let trait_type_args: Vec<crate::tir::TypeId> = match &impl_block.trait_type {
                Some(ast::Type::Generic(generic)) => generic
                    .args
                    .iter()
                    .map(|arg| scope.resolve_type(arg))
                    .collect(),
                _ => Vec::new(),
            };
            // Concrete-impl owner (`impl List<u8>`): the receiver's
            // qualified mangle, matching call sites (issue #1348).
            let concrete_owner: Option<crate::name::FqTypeName> =
                if scope.impl_is_concrete_instantiation(&impl_block.ty) {
                    let tt = scope.tysys.type_table.borrow();
                    let peeled = tt.peel_refs(self_type);
                    let is_instantiation = match tt.get(peeled) {
                        crate::tir::ResolvedType::GenericInstance { .. } => true,
                        crate::tir::ResolvedType::Newtype { type_args, .. } => {
                            // A trait impl needs none: the trait index keys it.
                            !type_args.is_empty() && trait_name.is_none()
                        }
                        _ => false,
                    };
                    is_instantiation.then(|| tt.fq_type_name(peeled))
                } else {
                    None
                };
            scope.record_impl_facts(
                impl_block.id,
                sem::types::ImplFacts {
                    trait_name: trait_name.clone(),
                    trait_type_args,
                    is_handler_method,
                    is_ref_impl,
                    struct_name: qualified_struct_name,
                    receiver,
                    concrete_owner,
                },
            );
        }

        // Reify reads a constant's body facts under this module's
        // perspective, so this module must be the one that records them.
        let mut scope = scope;
        for constant in &impl_block.constants {
            let declared = scope.resolve_type(&constant.ty);
            let mut ctx = crate::elaborator::types::FunctionContext::new(
                declared,
                crate::name::global_name(&scope.current_module_source, &constant.name),
            );
            scope.resolve_expr(&constant.value, &mut ctx, Some(declared));
        }

        // Collect explicitly provided method names
        let provided_method_names: Vec<String> =
            impl_block.methods.iter().map(|m| m.name.clone()).collect();

        let impl_is_concrete = scope.impl_is_concrete_instantiation(&impl_block.ty);
        for method in &impl_block.methods {
            // Records-only: reify emits the method `TirFunction`
            // from the recorded signature facts + the AST.
            let method_def = scope.def_at(method.id);
            let recorded_sig = scope
                .tysys
                .signatures
                .method_sig(method_def)
                .cloned()
                .expect("the decl pass records every impl-declared method's canonical signature");
            scope.resolve_method(
                method,
                &struct_name,
                &impl_block.ty,
                trait_name.as_ref(),
                impl_block.trait_type.as_ref(),
                impl_is_concrete,
                &impl_block.type_params,
                Some(&recorded_sig),
            );
        }

        // For trait impls, synthesize TIR functions for default methods
        // not explicitly provided in the impl block. `trait_name`
        // carries the mangled form ("Maker<i32>") used for method
        // naming; the trait declaration itself is indexed by its
        // base name ("Maker"), so we derive that from the AST.
        if let (Some(trait_n), Some(trait_ast)) =
            (trait_name.as_ref(), impl_block.trait_type.as_ref())
        {
            let trait_decl_name = scope.get_type_name(trait_ast);
            let default_methods: Vec<std::rc::Rc<ast::Function>> = scope
                .trait_sig_by_name(&trait_decl_name)
                .map(|trait_sig| {
                    trait_sig
                        .default_methods()
                        .filter(|(name, _)| {
                            !provided_method_names
                                .iter()
                                .any(|provided| provided == name)
                        })
                        .map(|(_, body)| std::rc::Rc::clone(body))
                        .collect()
                })
                .unwrap_or_default();

            // A default method's body is foreign AST owned by the trait module,
            // and one `AstId` serves N impls, so each needs its own
            // `ModuleSemantics` snapshot on `default_method_semantics` or the
            // synthesis would overwrite its own per-node facts. `decls` and
            // `imports` are cloned from the impl module for name resolution.
            for default_method in &default_methods {
                // Build a synthetic `ModuleSemantics` for this
                // one (impl, default_method) synthesis. Fresh
                // `types` / `bindings` so the body walk's writes
                // stay isolated; clone the impl module's `decls`
                // / `imports` so the walk's reads see the
                // resolved decls + import context.
                let synthetic = super::elaborator::sem::ModuleSemantics {
                    bindings: super::elaborator::sem::ModuleBindings::default(),
                    imports: scope.sem.imports.clone(),
                    types: super::elaborator::sem::TypeAnnotations::default(),
                    decls: scope.sem.decls.clone(),
                    default_method_semantics: crate::hashmap::IndexMap::default(),
                };
                // Swap the elaborator's owned `sem` with the
                // synthetic. `resolve_method` writes through
                // `scope.sem` (`record_*` calls, fact insertions)
                // — they all land in `synthetic`'s fresh maps.
                // Its `TirFunction` return is discarded; reify
                // emits the authoritative TIR from the recorded
                // facts.
                let saved_sem = std::mem::replace(&mut scope.sem, synthetic);

                let _ = scope.resolve_method(
                    default_method,
                    &struct_name,
                    &impl_block.ty,
                    Some(trait_n),
                    Some(trait_ast),
                    impl_is_concrete,
                    &impl_block.type_params,
                    None,
                );

                // Swap back, take the populated synthetic out.
                let mut populated = std::mem::replace(&mut scope.sem, saved_sem);

                // Drain decl-level writes that must flow back
                // into the impl module's `TirModule`. The body
                // walk's only such write is anon-struct push;
                // synthesis-request pushes only happen at the
                // decl pass, not inside a method body.
                scope
                    .sem
                    .decls
                    .pending_anonymous_structs
                    .append(&mut populated.decls.pending_anonymous_structs);

                // Stash the populated synthetic under the
                // (impl, default_method) key so reify can swap
                // `scope.sem` to it during its synthesis pass.
                scope
                    .sem
                    .default_method_semantics
                    .insert((impl_block.id, default_method.id), populated);
            }
        }
    }
}
