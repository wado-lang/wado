//! Type resolution phase for Wado
//!
//! The type elaborator:
//! 1. Takes the parsed AST and symbol table from the analyzer
//! 2. Performs type inference and type checking
//! 3. Produces the Typed Intermediate Representation (TIR)
//!
//! All type resolution happens in this phase. The output TIR has fully
//! resolved types on every expression, making code generation mechanical.

mod assert;
mod call;
mod callee;
mod closure;
mod coercion;
mod control_flow;
mod expr;
mod handlers;
mod infer;
mod item;
pub(crate) mod liveness;
mod matches;
mod method_call;
mod method_lookup;
mod module;
mod operators;
pub(crate) mod orchestration;
pub(crate) mod reify;
pub(crate) mod sem;
mod stmt;
mod template;
pub(crate) mod trait_env;
mod trait_query;
mod type_resolution;
mod typecheck;
pub(crate) mod types;
mod tysys;
mod util;

use std::cell::RefCell;
use std::rc::Rc;

use crate::hashmap::{IndexMap, IndexSet};

use crate::ast::{self, Item, Module};
use crate::compiler_host::CompilerHost;
use crate::logger::{Bail, Logger};
use crate::module_source::{ModuleSource, ModuleSourceInterner};
use crate::name::{self as name, MethodName};
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

/// `true` if `name` denotes a built-in primitive Wado type. Primitives
/// have inherent impl blocks in `core:prelude/primitive` but no
/// `Symbol` entry to canonicalise through, so both
/// [`Elaborator::canonical_decl_key`] and `TraitEnv::build` consult this
/// predicate to map a bare primitive reference to its canonical
/// declaring module.
pub(crate) fn is_primitive_type_name(name: &str) -> bool {
    matches!(
        name,
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
    )
}
pub use types::TypeError;
use types::{
    EnumInfo, FlagsInfo, GenericNewtypeInfo, ResourceInfo, StructFieldInfo, TypeLookup, VariantInfo,
};

// Migration markers below trace the WEP 2026-05-26 elaborator
// re-architecture. Each `MIGRATION:` line names the field's future home —
// `TypeSystem` (pipeline-wide type knowledge) or a sub-struct of
// `ModuleSemantics` (per-module facts), or a transient annotate-time scope
// or cross-cutting input that stays on the per-module driver.

pub struct Elaborator<'a, H: CompilerHost> {
    /// Pipeline-wide type knowledge: type arena, decl-interned type
    /// tables, registries, included-files map, and the read-only caches
    /// built once during `annotate_modules`. See [`tysys::TypeSystem`].
    pub(crate) tysys: tysys::TypeSystem,
    /// Per-module semantic facts (imports, decls, bindings, type
    /// annotations). The elaborator takes ownership of one
    /// [`sem::ModuleSemantics`] at the start of [`Self::resolve_module`]
    /// and the driver re-installs it into
    /// [`orchestration::AnnotateState::module_semantics`] afterwards. See
    /// the [`sem`] module-level documentation for the membership rules.
    pub(crate) sem: sem::ModuleSemantics,
    /// Symbol table from analyzer
    symbols: &'a SymbolTable,
    /// Loaded modules from analyzer
    loaded_modules: &'a IndexMap<ModuleSource, Module>,
    /// Logger for emitting diagnostics
    logger: &'a Logger<'a, H>,
    /// Current module source being resolved (for struct type `module_source`)
    // MIGRATION: identifies the active `ModuleSemantics` — the driver swaps
    //   modules via the `IndexMap<ModuleSource, ModuleSemantics>` key, so this
    //   field eventually disappears (Stage 5 carries it as an explicit
    //   argument to the per-module walker).
    current_module_source: ModuleSource,
    /// Entry module source (for cross-module import dedup)
    entry_module_source: ModuleSource,
    /// Current module items (for local function parameter lookup)
    current_module_items: &'a [Item],
    /// Mutable trait resolution context: type params, bounds, associated type bindings, self type.
    /// Grouped together so scope entry/exit can save/restore the whole context at once.
    // MIGRATION: transient annotate-time scope (Stage 5 carries it as an
    //   explicit argument to `annotate_*` walkers).
    trait_ctx: trait_env::TraitContext,
    /// Effect parameter names currently in scope (from enclosing function's `<effect E>`)
    // MIGRATION: transient annotate-time scope (per-function, not per-module).
    current_effect_params: IndexSet<String>,
    /// Map from effect parameter name to its declaration `AstId` in the current scope.
    /// Used to record use→def edges for effect parameter references in `with` clauses.
    // MIGRATION: transient annotate-time scope (per-function, not per-module).
    current_effect_param_decls: IndexMap<String, crate::ast::AstId>,
    /// Recursion guard for `type_implements_trait` to avoid infinite recursion
    /// on recursive types (e.g., variant Elem containing struct `RepeatElem` with field Elem).
    /// Frames are pushed on entry and popped on return; the stack is empty
    /// at every quiescent point.
    // MIGRATION: transient annotate-time scope. Despite the `RefCell<Vec<…>>`
    //   shape this is NOT a type-system cache (whose entries would persist
    //   across calls) — it is a per-call frame stack. Sharing it across
    //   modules via `TypeSystem` would either leak stale frames (producing
    //   wrong "recursive, optimistically true" answers from
    //   `type_implements_trait` — a soundness bug) or require per-call
    //   save/restore plumbing that defeats the move. Belongs with
    //   `trait_ctx` in Stage 5's per-function walker argument bag, not on
    //   `TypeSystem`. See `tysys.rs` module docs ("Deferred fields") for
    //   the full rationale.
    trait_check_stack: RefCell<Vec<(TypeId, String)>>,
    /// When resolving a default-expression AST at a call site, fall back to
    /// looking up unresolved identifiers in this module's global scope. This
    /// preserves the callee's lexical scope for defaults that reference
    /// module-private items (see WEP 2026-04-11).
    // MIGRATION: transient annotate-time scope (per-call-site override).
    pub(super) default_scope_module: Option<ModuleSource>,
    /// Kiln invocation redirects consulted by `use` resolution sites. Shared
    /// by `Rc` so per-module Elaborator instances can read the single
    /// compilation-unit-wide redirect map cheaply.
    pub(super) invocations: Rc<crate::kiln::InvocationIndex>,
    /// `ModuleSource` interner shared with the loader and downstream
    /// phases. Wrapped in `Rc<RefCell<>>` so per-module elaborator
    /// instances can `borrow_mut()` it from `&self` contexts (e.g.
    /// `record_use_specifier_references`).
    pub(super) interner: Rc<RefCell<ModuleSourceInterner>>,
    /// Side-channel populated by [`Self::resolve_method_call_with`] for the
    /// successful-dispatch case, carrying the `(self_kind, is_ref_impl,
    /// FunctionRef)` the dispatch resolved. Synthetic callers (e.g. for-of's
    /// `.into_iter()` / `.next()`) read it back to record the dispatch info
    /// their own way — they pass `call_id == None` so the in-function
    /// `record_method_dispatch` call is skipped, and reify needs the
    /// recorded receiver-adjustment inputs + the resolved `FunctionRef` all
    /// the same (Gap 6 of WEP 2026-05-26 §`Design notes (Stage 5)`).
    /// Since Stage 7-B `resolve_method_call_with` returns only a typed
    /// placeholder, the `FunctionRef` travels through this channel rather
    /// than being recovered from the returned `MethodCall` TIR.
    ///
    /// Cleared at the top of `resolve_method_call_with` so a synthetic
    /// caller never accidentally reads a stale value from a previous
    /// dispatch. `None` means either no dispatch ran (the short-circuit
    /// paths returned early) or the method-not-found recovery branch
    /// took over.
    pub(super) pending_method_dispatch: Option<(ast::SelfKind, bool, tir::FunctionRef)>,
    /// Side-channel populated by [`Self::resolve_binary`] and the
    /// `Expr::Index` arm of [`Self::resolve_expr`] before they call
    /// the trait-operator dispatcher, carrying the source-level
    /// [`crate::ast::BinaryExpr`] / [`crate::ast::IndexExpr`] id that
    /// reify needs to key the `OperatorDispatch` record on (Gap 11
    /// of WEP 2026-05-26 §`Design notes (Stage 5)`).
    ///
    /// [`Self::build_trait_op_method_call_on_resolved`] reads this
    /// channel, records the dispatch decision via
    /// [`Self::record_operator_dispatch`], and clears it. Synthesised
    /// binary calls (e.g. inside `desugar_comparison_chain`) leave the
    /// channel `None`, and the recording is skipped — those calls have
    /// no source AST id reify could key on.
    pub(super) pending_operator_ast_id: Option<crate::ast::AstId>,
    /// When `true`, [`Self::resolve_tuple_for_of`] captures each unrolled
    /// element's body annotations into
    /// [`super::sem::types::TypeAnnotations::tuple_overlays`] and
    /// truncates the per-element maps back to their pre-loop length so
    /// each element starts from a clean slate. Set only when reify will
    /// consume the result (Stage 5); `false` for the production / LSP
    /// path so their annotation maps are left exactly as before.
    pub(super) capture_tuple_overlays: bool,
    /// When `true`, the single use→def edge sink [`Self::insert_reference`]
    /// (which every `record_*` helper funnels through) drops edges instead of
    /// recording them. Set while a *type-checking query* resolves a
    /// declaration's signature whose AST nodes belong to another declaration —
    /// most notably [`Self::lookup_method_info`], which resolves a method's
    /// parameter / return types from a (possibly foreign) `impl` / `resource`
    /// declaration to compute its `MethodInfo`.
    ///
    /// Those signature nodes are owned by the declaring module and already get
    /// their use→def edges recorded when that module is annotated
    /// (`resolve_resource_decl`, the impl-method walk). Globally-unique
    /// `AstId`s mean re-recording can no longer clobber an unrelated node's
    /// edge, but a query re-resolution may still record a *different* def
    /// than the owning module's walk did (it resolves under the consumer's
    /// import scope), so queries stay suppressed to keep the owning walk's
    /// edge authoritative.
    pub(super) suppress_reference_recording: bool,
}

impl<'a, H: CompilerHost> Elaborator<'a, H> {
    /// Construct a [`TypeLookup`] view over the elaborator's current import
    /// context and shared `all_*` tables. Use this for any type-name
    /// resolution; never reach into `all_*` directly.
    pub(crate) fn type_lookup(&self) -> TypeLookup<'_> {
        TypeLookup {
            current_module_source: &self.current_module_source,
            imported_type_sources: &self.sem.imports.imported_type_sources,
            import_original_names: &self.sem.imports.import_original_names,
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
        }
    }

    /// Canonicalize a `<ns>::<member>` reference (single `::`, prefix is a
    /// namespace import alias) to the bare `<member>` form. Returns `None`
    /// when the name isn't of that shape — including multi-segment cases like
    /// `<ns>::<Type>::<case>`, which the elaborator routes through dedicated
    /// namespace paths (see `resolve_ident` / `resolve_call`).
    ///
    /// The AST keeps the user-written `ns::member` so LSP cursors land on it
    /// as typed; the name lookups against `imported_functions`,
    /// `imported_globals`, struct registries, etc. see the canonical form
    /// they were populated with.
    pub(super) fn strip_ns_prefix<'s>(&self, name: &'s str) -> Option<&'s str> {
        self.sem.imports.strip_ns_prefix(name)
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

    /// Run `body` with the elaborator's "current module" perspective swapped to
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
        let saved_imp = std::mem::replace(
            &mut self.sem.imports.imported_type_sources,
            imported_type_sources,
        );
        let saved_orig = std::mem::replace(
            &mut self.sem.imports.import_original_names,
            import_original_names,
        );
        let saved_local_struct = std::mem::take(&mut self.sem.decls.local_struct_fields);
        let saved_local_newtypes = std::mem::take(&mut self.sem.decls.local_newtypes);
        let saved_local_enum = std::mem::take(&mut self.sem.decls.local_enum_cases);
        let saved_local_flags = std::mem::take(&mut self.sem.decls.local_flags_cases);
        let saved_local_gnt = std::mem::take(&mut self.sem.decls.local_generic_newtypes);
        let saved_local_variant = std::mem::take(&mut self.sem.decls.local_variant_cases);

        let result = body(self);

        self.current_module_source = saved_src;
        self.sem.imports.imported_type_sources = saved_imp;
        self.sem.imports.import_original_names = saved_orig;
        self.sem.decls.local_struct_fields = saved_local_struct;
        self.sem.decls.local_newtypes = saved_local_newtypes;
        self.sem.decls.local_enum_cases = saved_local_enum;
        self.sem.decls.local_flags_cases = saved_local_flags;
        self.sem.decls.local_generic_newtypes = saved_local_gnt;
        self.sem.decls.local_variant_cases = saved_local_variant;
        result
    }

    /// Run `body` with use→def reference recording suppressed, restoring the
    /// previous setting on return. Used by type-checking queries that resolve
    /// foreign declaration signatures (see
    /// [`Self::suppress_reference_recording`]).
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

    /// Record that an identifier resolved to a declared symbol reachable from
    /// the current module under `name` (local item, imported item, imported
    /// namespace member, etc.). Looks up the defining [`AstId`](crate::ast::AstId) through
    /// the symbol table; no-op if the name is not declared.
    pub(super) fn record_item_reference_by_name(&mut self, use_id: crate::ast::AstId, name: &str) {
        let Some(sym) = self
            .symbols
            .lookup_in_module(&self.current_module_source, name)
            .or_else(|| self.symbols.lookup(&self.current_module_source, name))
        else {
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
    pub(super) fn record_type_name_reference(&mut self, use_id: crate::ast::AstId, name: &str) {
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

    /// Build a [`control_flow::CtrlFlowCtx`] over the currently-active
    /// module's `expression_types` map. Used by the AST-level
    /// missing-return / definite-exit walks that replaced the TIR
    /// walkers consuming the combined walk's body TIR.
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
    /// than a built `TirBlock`. AST-level replacement for
    /// `Self::block_result_type(&TirBlock)` (Stage 7-B): lets the combined
    /// walk type `{ … }`, `if`/`match` arms, loop and handler bodies
    /// without inspecting the body TIR it produces.
    pub(super) fn ast_block_result_type(&self, block: &crate::ast::Block) -> TypeId {
        control_flow::block_result_type(self.ctrl_flow_ctx(), block)
    }

    /// Emit a `MissingReturn` diagnostic when a declared non-Unit
    /// return type cannot be satisfied by the body. Skipped for
    /// `Unit`/`Never` declarations, for missing bodies (declared-only
    /// externals), and for bodies whose every control path provably
    /// exits before the end — either via an explicit `return`, a
    /// divergent statement like `panic("…")`, or a fully-diverging
    /// labeled block / `loop`.
    ///
    /// The previous heuristic accepted any nested `return` even when
    /// only one branch of an `if` carried one, which let partial-return
    /// bodies through and produced an invalid core Wasm module ("type
    /// mismatch: expected i32 but nothing on stack") for the
    /// fall-through path. AST-walker mirrors that strict definite-exit
    /// analysis using recorded `expression_types`.
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

    /// Record the resolved [`TypeId`] for the expression at `ast_id` —
    /// the per-`AstId` annotation the future `reify` pass (Stage 5 of WEP
    /// 2026-05-26) reads to set `TirExpr::type_id` without re-running
    /// inference.
    ///
    /// Skipped when the resolution failed (yielding [`TypeTable::ERROR`])
    /// or when the recorded type still mentions
    /// [`TypeTable::UNKNOWN`]. Both signal a result the body walk is
    /// going to revisit (the unknown-typed `null` literal is patched up
    /// once the surrounding `Option<T>` becomes known by
    /// [`crate::elaborator::expr::patch_unresolved_null`]); recording
    /// the sentinel here would leave a stale entry the reify pass cannot
    /// consume and the post-patch TIR would disagree with.
    pub(super) fn record_expression_type(&mut self, ast_id: crate::ast::AstId, type_id: TypeId) {
        if type_id == TypeTable::ERROR {
            return;
        }
        // UNKNOWN-containing types ARE recorded (Stage 7-B): a bare `null` is
        // `Option<UNKNOWN>`, and the AST-level block-result-type analysis the
        // combined walk uses (in place of reading the body TIR) needs to see
        // an unresolved-null branch to type it the same way the TIR walker
        // did. Readers that want a *definite* type filter `contains_unknown`
        // explicitly: reify's `ann_expression_types` (so a null still falls
        // back to its `expected_type`) and the missing-return walk in
        // `control_flow.rs`.
        self.sem.types.expression_types.insert(ast_id, type_id);
    }

    /// Record a method-dispatch decision for the [`crate::ast::MethodCallExpr`]
    /// at `ast_id`. Centralised here (rather than at the AST wrapper) so
    /// every TIR-construction path that flows through
    /// [`Self::build_tir_method_call`] — including the `container[i].method()`
    /// `IndexMut` rewrite — leaves a single, uniform entry in
    /// [`sem::types::TypeAnnotations::method_dispatch`].
    ///
    /// Synthetic calls (for-of's `.into_iter()` / `.next()`) pass
    /// `ast_id == None` and skip recording per the
    /// [`sem::types::MethodDispatch`] contract.
    ///
    /// `is_ref_impl` is the flag `lookup_method_info` produces alongside
    /// the dispatch target; reify uses it together with `self_kind` to
    /// drive `adjust_receiver_for_self_kind` without re-running impl
    /// lookup (Gap 2 of Stage 5).
    pub(super) fn record_method_dispatch(
        &mut self,
        ast_id: Option<crate::ast::AstId>,
        function_ref: &tir::FunctionRef,
        self_kind: ast::SelfKind,
        is_ref_impl: bool,
        param_is_mut: Vec<bool>,
        param_names: Vec<String>,
        param_defaults: Vec<Option<ast::Expr>>,
        return_type: TypeId,
        method_type_args: Vec<TypeId>,
    ) {
        let Some(ast_id) = ast_id else { return };
        let key = ast_id;
        self.sem.types.method_dispatch.insert(
            key,
            sem::types::MethodDispatch {
                function_ref: function_ref.clone(),
                self_kind,
                is_ref_impl,
                param_is_mut,
                param_names,
                param_defaults,
                return_type,
                method_type_args,
            },
        );
    }

    /// Record a generic-instantiation decision for the call / struct
    /// literal / variant-ctor at `ast_id` (Gap 1 of Stage 5). `type_args`
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
    /// `ast_id` (Gap 4 of Stage 5). See [`sem::types::ClosureCaptureInfo`].
    pub(super) fn record_closure_captures(
        &mut self,
        ast_id: crate::ast::AstId,
        info: sem::types::ClosureCaptureInfo,
    ) {
        let key = ast_id;
        self.sem.types.closure_captures.insert(key, info);
    }

    /// Record the power-assert capture-slot table for the assert
    /// statement at `ast_id` (Gap 5 of Stage 5). See
    /// [`sem::types::AssertCaptureInfo`].
    pub(super) fn record_assert_captures(
        &mut self,
        ast_id: crate::ast::AstId,
        info: sem::types::AssertCaptureInfo,
    ) {
        let key = ast_id;
        self.sem.types.assert_captures.insert(key, info);
    }

    /// Record the iterator-path dispatch decision for the for-of
    /// statement at `ast_id` (Gap 6 of Stage 5). Tuple / variadic paths
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
    /// (Gap 11 of Stage 5). Absence of a recorded entry signals to
    /// reify that the native
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

    /// Record the handler-binding resolution facts (Gap 13 of
    /// Stage 5) keyed by the
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

    /// Record the impl-block resolution facts (Gap 12 of Stage 5)
    /// keyed by the [`crate::ast::ImplBlock`]'s [`AstId`]. Reify
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

    /// Record an `impl Trait for Type;` synthesis request (Gap 12 /
    /// Stage 5). `reify_module` reads
    /// [`sem::decls::ModuleDecls::pending_synthesis_requests`] and
    /// pushes each onto the emitted [`tir::TirModule::synthesis_requests`].
    pub(super) fn record_pending_synthesis_request(&mut self, req: tir::SynthesisRequest) {
        self.sem.decls.pending_synthesis_requests.push(req);
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
    /// at `ast_id` (Stage 7-B). Called from [`Self::resolve_ident`] at each
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
            span: Some(span),
        };
        self.sem.bindings.local_symbols.insert(def_id, symbol);
        self.sem.types.local_types.insert(def_id, type_id);
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
    ///
    /// Consults [`tysys::TypeSystem::loaded_module_func_indices`] — the
    /// per-module name→item-index map built once during annotate — to
    /// avoid rebuilding the index at every `resolve_module` call.
    fn lookup_current_func(&self, func_name: &str) -> Option<&ast::Function> {
        let idx_map = self
            .tysys
            .loaded_module_func_indices
            .get(&self.current_module_source)?;
        let &idx = idx_map.get(func_name)?;
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
        self.sem
            .decls
            .local_struct_fields
            .get(name)
            .filter(|info| info.module_source == *module_source)
            .or_else(|| {
                self.tysys
                    .all_struct_fields
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
        interner: &mut ModuleSourceInterner,
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

    /// Resolve a bare declaration name (effect / resource / trait / type)
    /// referenced in the current module to its canonical
    /// `(declaring module, name)` key. The decl indices on [`TraitEnv`] are
    /// keyed by this canonical pair so two modules can host same-named
    /// declarations without colliding; every lookup site must canonicalise
    /// the name through this helper before consulting the index.
    ///
    /// Resolution order, in priority:
    /// 1. `imported_type_sources` — per-module `use { Foo as Bar } from "…"`
    ///    declarations, with `import_original_names` so an aliased import
    ///    canonicalises to the original declaration name rather than the
    ///    local alias.
    /// 2. `effect_sources` — local `interface` / `resource` definitions in
    ///    the current module (and non-aliased imports). Consulted second so
    ///    aliased effect/resource imports go through the alias-aware path
    ///    above.
    /// 3. The current module — a name *defined* here shadows the symbol-table
    ///    and decl-index fallbacks below, so a local declaration always wins
    ///    over an unrelated same-named item in another module (issue #1298).
    /// 4. The symbol table, scoped to the current module's imports plus the
    ///    implicit prelude — canonicalises explicitly imported names and
    ///    prelude/stdlib names to their defining module.
    /// 5. The global decl indices on [`TraitEnv`] — last-resort fallback
    ///    for prelude traits referenced from stdlib code where neither
    ///    the per-module import context nor the symbol table carries the
    ///    binding (prelude is implicit, not threaded through `use`).
    ///
    /// When the canonicalised name had an alias (`use { Foo as Bar }`),
    /// the returned key uses the *original* declaration name, so the index
    /// (whose key is `(decl_module, decl_name)`) matches.
    pub(crate) fn canonical_decl_key(&self, name: &str) -> (ModuleSource, String) {
        super::elaborator::trait_query::canonical_decl_key_with(
            name,
            &self.current_module_source,
            &self.sem.imports,
            self.symbols,
            &self.tysys.trait_env,
        )
    }

    /// Canonical decl identity `(module, name)` of the declared type behind
    /// `type_id` (refs peeled), or `None` for primitives / tuples / functions
    /// and other types that carry no declaring module. Unlike a bare
    /// `type_name`, this keeps two same-named types from different modules
    /// (e.g. `core:temporal::Instant` vs `wasi:clocks::Instant`) distinct.
    /// For a generic instance it is the *base* type's identity — type arguments
    /// are dropped, so it cannot tell `Foo<A>` from `Foo<B>`.
    pub(crate) fn type_decl_key(&self, type_id: tir::TypeId) -> Option<(ModuleSource, String)> {
        use crate::tir::ResolvedType;
        let tt = self.tysys.type_table.borrow();
        match tt.get(tt.peel_refs(type_id)) {
            ResolvedType::Struct {
                name,
                module_source,
                ..
            }
            | ResolvedType::Enum {
                name,
                module_source,
            }
            | ResolvedType::Variant {
                name,
                module_source,
            }
            | ResolvedType::Flags {
                name,
                module_source,
            }
            | ResolvedType::Resource {
                name,
                module_source,
            }
            | ResolvedType::Newtype {
                name,
                module_source,
                ..
            }
            | ResolvedType::GenericInstance {
                name,
                module_source,
                ..
            }
            | ResolvedType::GenericResource {
                name,
                module_source,
                ..
            } => Some((module_source.clone(), name.clone())),
            _ => None,
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
                if let Some(&decl_id) = self.current_effect_param_decls.get(name) {
                    if let Some(use_id) = use_id {
                        self.record_reference(use_id, decl_id);
                    }
                    tir::EffectRef::Param { name: name.clone() }
                } else if let Some(source) = self.sem.imports.effect_sources.get(name).cloned() {
                    // Canonicalize: re-exports point at the importing module, but
                    // identity must match the defining module so two `with Stdout`
                    // clauses (one importing from `core:cli`, one from `wasi:cli`)
                    // refer to the same effect.
                    let canonical = self
                        .symbols
                        .lookup_in_module(&source, name)
                        .map(|sym| {
                            if let Some(use_id) = use_id {
                                self.record_reference_to_def(use_id, sym.defined_at);
                            }
                            sym.module_source().clone()
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
                        .lookup(&self.current_module_source, name)
                        .map(|sym| {
                            if let Some(use_id) = use_id {
                                self.record_reference_to_def(use_id, sym.defined_at);
                            }
                            sym.module_source().clone()
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

    /// Resolve a module, converting AST to TIR
    /// Walk a module for its fact-recording side effects (Stage 7-B:
    /// records-only / `annotate_bodies`). reify (`reify_module`) is the sole
    /// producer of the `TirModule`; this walk resolves every item body
    /// (recording types / dispatch / signatures / desugar facts and emitting
    /// diagnostics) so reify and the LSP can read them. Returns `Bail` on a
    /// fatal logger error, matching the previous TIR-building contract.
    pub fn resolve_module(
        &mut self,
        module: &'a Module,
        module_source: ModuleSource,
    ) -> Result<(), Bail> {
        // Set current module source for struct type creation
        self.current_module_source = module_source.clone();
        // Store current module items as a reference (no clone)
        self.current_module_items = &module.items;
        // Function name → item-index lookup is pre-built per loaded
        // module on `tysys.loaded_module_func_indices` during annotate;
        // `lookup_current_func` consults it through
        // `self.current_module_source` so no per-resolve rebuild here.
        // Build effect source map from imports
        self.sem.imports.effect_sources = Self::build_effect_sources(
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
            let _span = self.logger.span("elaborate/collect_types");
            self.collect_types(module);
        }

        // Second pass: collect function signatures (for call resolution)
        {
            let _span = self.logger.span("elaborate/collect_sigs");
            self.collect_function_signatures(module);
        }

        // Collect global variable names and types (before resolving functions that may reference them)
        self.sem.decls.current_module_globals.clear();
        self.sem.decls.imported_globals.clear();
        for item in &module.items {
            if let Item::Global(global_decl) = item {
                let ty = self.resolve_type(&global_decl.ty);
                self.sem
                    .decls
                    .current_module_globals
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

                // Collect `(local_name, source_global_name)` pairs to import:
                // a `Simple` import names one global; a `Namespace` import
                // brings every public global into scope under its `ns$global`
                // alias.
                if let Some(source_module) = self.loaded_modules.get(&source_module_source) {
                    let mut to_import: Vec<(String, String)> = Vec::new();
                    for use_item in &use_decl.items {
                        match use_item {
                            ast::UseItem::Simple { name, alias, .. } => {
                                if let Some(symbol) =
                                    self.symbols.lookup_in_module(&source_module_source, name)
                                    && matches!(symbol.kind, crate::symbol::SymbolKind::Global(_))
                                {
                                    to_import.push((
                                        alias.as_ref().unwrap_or(name).clone(),
                                        name.clone(),
                                    ));
                                }
                            }
                            ast::UseItem::Namespace { name: ns } => {
                                // Each public global is imported under its
                                // `ns$global` alias.
                                for src_item in &source_module.items {
                                    if let Item::Global(global_decl) = src_item
                                        && global_decl.is_pub
                                    {
                                        to_import.push((
                                            crate::name::namespace_member_alias(
                                                ns,
                                                &global_decl.name,
                                            ),
                                            global_decl.name.clone(),
                                        ));
                                    }
                                }
                            }
                            ast::UseItem::InterfaceFunctions { .. } | ast::UseItem::Wildcard => {}
                        }
                    }
                    for (local_name, source_name) in to_import {
                        // Find the global declaration in the source module to
                        // resolve its type and mutability.
                        for src_item in &source_module.items {
                            if let Item::Global(global_decl) = src_item
                                && global_decl.name == source_name
                            {
                                let ty = self.resolve_type(&global_decl.ty);
                                self.sem.decls.imported_globals.insert(
                                    local_name,
                                    (
                                        source_module_source.clone(),
                                        source_name,
                                        ty,
                                        global_decl.mutable,
                                    ),
                                );
                                break;
                            }
                        }
                    }
                }
            }
        }

        // Collect associated constants from loaded modules and current
        // module, tagging each with its defining `ModuleSource` so reify can
        // reify the body under the right module perspective (the body's
        // `AstId`s are only unique within that module).
        //
        // Resolve the const's type here, in the current module's perspective,
        // so reify and the combined walk both read a ready `TypeId` instead
        // of re-running `resolve_type` at every use site. The resolution
        // context matches what the use sites would use today (assoc-const
        // types are primitive or `Self`-substituted and resolve uniformly
        // across modules).
        self.sem.decls.associated_constants.clear();
        let assoc_const_inputs: Vec<(ModuleSource, String, ast::Type, ast::Expr)> = self
            .loaded_modules
            .iter()
            .map(|(src, m)| (src.clone(), &m.items))
            .chain(std::iter::once((module_source, &module.items)))
            .flat_map(|(src, module_items)| {
                module_items
                    .iter()
                    .filter_map(|item| {
                        if let Item::Impl(impl_block) = item {
                            Some((src.clone(), impl_block))
                        } else {
                            None
                        }
                    })
                    .flat_map(|(src, impl_block)| {
                        let type_name = self.get_type_name(&impl_block.ty);
                        impl_block
                            .constants
                            .iter()
                            .map(move |assoc_const| {
                                (
                                    src.clone(),
                                    MethodName::format_local(&type_name, None, &assoc_const.name),
                                    assoc_const.ty.clone(),
                                    assoc_const.value.clone(),
                                )
                            })
                            .collect::<Vec<_>>()
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
        // Also register each constant reachable through a namespace import
        // under its `ns$Type::member` alias, so `ns::Type::CONST` (which
        // canonicalizes to `ns$Type::CONST`) resolves to the constant in that
        // namespace's module.
        let ns_aliases: Vec<(String, ModuleSource)> = self
            .sem
            .imports
            .namespace_imports
            .iter()
            .map(|(ns, src)| (ns.clone(), src.clone()))
            .collect();
        for (src, key, ty, value) in assoc_const_inputs {
            let type_id = self.resolve_type(&ty);
            for (ns, ns_src) in &ns_aliases {
                if *ns_src == src {
                    self.sem.decls.associated_constants.insert(
                        name::namespace_member_alias(ns, &key),
                        (src.clone(), type_id, value.clone()),
                    );
                }
            }
            self.sem
                .decls
                .associated_constants
                .insert(key, (src, type_id, value));
        }

        // Third pass: resolve functions
        let _resolve_funcs_span = self.logger.span("elaborate/resolve_funcs");

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
                        if self.tysys.is_known_type_name(&param.name) {
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
                                    && !self.tysys.is_known_type_name(name)
                                {
                                    let type_id = self
                                        .tysys
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

                    // `impl Trait for Type;` — record synthesis request and skip.
                    // The request lands on `sem.decls.pending_synthesis_requests`;
                    // `reify_module` reads it and emits the
                    // `tir_module.synthesis_requests`.
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
                            let req = crate::tir::SynthesisRequest {
                                trait_name: synth_trait_name,
                                target_type_name: struct_name.clone(),
                                target_type_id,
                                type_params,
                                span: impl_block.span,
                            };
                            self.record_pending_synthesis_request(req);
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
                        let is_concrete = !self
                            .tysys
                            .type_table
                            .borrow()
                            .contains_type_param(target_type_id);

                        for binding in &impl_block.associated_types {
                            let type_id = self.resolve_type(&binding.ty);
                            self.trait_ctx
                                .assoc_type_bindings
                                .insert(binding.name.clone(), type_id);

                            // Register in TypeTable for substitution resolution
                            // Only for concrete types (not generic impls like impl<T> Trait for List<T>)
                            if is_concrete {
                                self.tysys
                                    .type_table
                                    .borrow_mut()
                                    .register_assoc_type_resolution(
                                        target_type_id,
                                        binding.name.clone(),
                                        type_id,
                                    );
                            } else {
                                // For generic impls, register the definition so the monomorphizer
                                // can resolve associated types for GenericInstance types.
                                self.tysys
                                    .type_table
                                    .borrow_mut()
                                    .register_generic_assoc_type_def(
                                        struct_name.clone(),
                                        binding.name.clone(),
                                        type_id,
                                    );
                            }
                        }
                    }

                    // Stage 5 / Gap 12: record the impl-block
                    // resolution facts so `reify_impl` can read them
                    // verbatim. All inputs are already computed by
                    // the setup above; the recording is one call
                    // that snapshots the resolved Self type, the
                    // trait canonical / mangled forms, the impl's
                    // TIR type-param projection, the assoc-type
                    // bindings, and the handler / ref-impl flags.
                    {
                        let self_type = self.resolve_type(&impl_block.ty);
                        let trait_canonical = impl_block.trait_type.as_ref().map(|t| {
                            let base = self.get_type_name(t);
                            self.canonical_decl_key(&base)
                        });
                        let is_handler_method = trait_canonical
                            .as_ref()
                            .map(|key| {
                                self.tysys.trait_env.effect_decl_index.contains_key(key)
                                    || self.tysys.trait_env.resource_decl_index.contains_key(key)
                            })
                            .unwrap_or(false);
                        let is_ref_impl = matches!(
                            &impl_block.ty,
                            ast::Type::Reference(_) | ast::Type::MutReference(_),
                        );
                        // Concrete type args of the impl's trait reference
                        // (`impl Future<i32>` → `[i32]`), resolved in the
                        // impl's type-param scope so generic impls
                        // round-trip their `TypeParam` ids. Mirrors
                        // `item.rs:1621`.
                        let trait_type_args: Vec<crate::tir::TypeId> = match &impl_block.trait_type
                        {
                            Some(ast::Type::Generic(generic)) => generic
                                .args
                                .iter()
                                .map(|arg| self.resolve_type(arg))
                                .collect(),
                            _ => Vec::new(),
                        };
                        // Per-instantiation owner for a fully concrete impl
                        // (`impl List<u8>`). Decided AST-side (excluding declared
                        // impl type params) so it agrees with method dispatch's
                        // `from_concrete_impl`; the name is built from the
                        // resolved `self_type` so it matches how call sites
                        // mangle the receiver (base name + per-arg mangle).
                        let concrete_owner: Option<String> = if self
                            .impl_is_concrete_instantiation(impl_block, &self.current_module_source)
                        {
                            let tt = self.tysys.type_table.borrow();
                            match tt.get(tt.peel_refs(self_type)) {
                                crate::tir::ResolvedType::GenericInstance {
                                    name,
                                    type_args,
                                    ..
                                } => {
                                    let args: Vec<String> =
                                        type_args.iter().map(|&a| tt.mangle_type_name(a)).collect();
                                    Some(format!("{}<{}>", name, args.join(",")))
                                }
                                _ => None,
                            }
                        } else {
                            None
                        };
                        self.record_impl_facts(
                            impl_block.id,
                            sem::types::ImplFacts {
                                trait_name_mangled: trait_name.clone(),
                                trait_canonical,
                                trait_type_args,
                                is_handler_method,
                                is_ref_impl,
                                struct_name: struct_name.clone(),
                                concrete_owner,
                            },
                        );
                    }

                    // Collect explicitly provided method names
                    let provided_method_names: Vec<String> =
                        impl_block.methods.iter().map(|m| m.name.clone()).collect();

                    let impl_is_concrete = self
                        .impl_is_concrete_instantiation(impl_block, &self.current_module_source);
                    for method in &impl_block.methods {
                        // Records-only: reify emits the method `TirFunction`
                        // from the recorded signature facts + the AST.
                        self.resolve_method(
                            method,
                            &struct_name,
                            &impl_block.ty,
                            trait_name.as_deref(),
                            impl_block.trait_type.as_ref(),
                            impl_is_concrete,
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
                        let trait_decl_name = self.get_type_name(trait_ast);
                        let (trait_methods, _) = self
                            .find_trait_decl_methods_with_module(&trait_decl_name)
                            .unzip();
                        let default_methods: Vec<ast::Function> = trait_methods
                            .unwrap_or_default()
                            .into_iter()
                            .filter(|m| {
                                m.body.is_some() && !provided_method_names.contains(&m.name)
                            })
                            .collect();

                        // A default method's body is *foreign* AST owned by
                        // the trait module; its nodes carry the trait
                        // module's globally-unique `AstId`s, so the facts the
                        // walk records cannot collide with impl-module nodes.
                        //
                        // Stage 5 completion: the body walk's only output
                        // is a per-impl `ModuleSemantics` snapshot stored on
                        // `ModuleSemantics::default_method_semantics`. Reify
                        // (the sole TIR source) reads those snapshots and
                        // produces the impl's default-method
                        // `TirFunction`s. The combined walk no longer builds
                        // default-method TIR.
                        //
                        // Per-impl snapshots are required because the same
                        // trait body synthesised for many impls would
                        // otherwise overwrite its own per-node facts on each
                        // iteration (one `AstId`, N impls). Each impl gets a
                        // fresh `ModuleSemantics` whose `types` / `bindings`
                        // capture only this one synthesis; `decls` and
                        // `imports` are cloned from the surrounding
                        // (impl-module) `ModuleSemantics` so name resolution
                        // / decl indices work during the body walk.
                        for default_method in &default_methods {
                            // Build a synthetic `ModuleSemantics` for this
                            // one (impl, default_method) synthesis. Fresh
                            // `types` / `bindings` so the body walk's writes
                            // stay isolated; clone the impl module's `decls`
                            // / `imports` so the walk's reads see the
                            // resolved decls + import context.
                            let synthetic = super::elaborator::sem::ModuleSemantics {
                                bindings: super::elaborator::sem::ModuleBindings::default(),
                                imports: self.sem.imports.clone(),
                                types: super::elaborator::sem::TypeAnnotations::default(),
                                decls: self.sem.decls.clone(),
                                default_method_semantics: crate::hashmap::IndexMap::default(),
                            };
                            // Swap the elaborator's owned `sem` with the
                            // synthetic. `resolve_method` writes through
                            // `self.sem` (`record_*` calls, fact insertions)
                            // — they all land in `synthetic`'s fresh maps.
                            // Its `TirFunction` return is discarded; reify
                            // emits the authoritative TIR from the recorded
                            // facts.
                            let saved_sem = std::mem::replace(&mut self.sem, synthetic);

                            let _ = self.resolve_method(
                                default_method,
                                &struct_name,
                                &impl_block.ty,
                                Some(trait_n),
                                Some(trait_ast),
                                impl_is_concrete,
                            );

                            // Swap back, take the populated synthetic out.
                            let mut populated = std::mem::replace(&mut self.sem, saved_sem);

                            // Drain decl-level writes that must flow back
                            // into the impl module's `TirModule`. The body
                            // walk's only such write is anon-struct push;
                            // synthesis-request pushes only happen at the
                            // decl pass, not inside a method body.
                            self.sem
                                .decls
                                .pending_anonymous_structs
                                .append(&mut populated.decls.pending_anonymous_structs);

                            // Stash the populated synthetic under the
                            // (impl, default_method) key so reify can swap
                            // `self.sem` to it during its synthesis pass.
                            self.sem
                                .default_method_semantics
                                .insert((impl_block.id, default_method.id), populated);
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
                    self.resolve_variant_decl(variant_decl);
                }
                Item::Test(test_decl) => {
                    // `test_index` only named the discarded combined-walk test
                    // function; reify re-indexes its own tests. Pass the running
                    // count for parity and resolve the body for its facts.
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
                // AST + decl tables, so the combined walk does nothing for them.
                Item::Enum(_) | Item::Flags(_) | Item::Newtype(_) => {}
                Item::Interface(effect_decl) => {
                    // Records `effect_ops`; reify reads them.
                    self.resolve_effect_decl(effect_decl);
                }
                Item::Resource(resource_decl) => {
                    self.resolve_resource_decl(resource_decl);
                }
                // Other items will be added as needed
                _ => {}
            }
        }

        drop(_resolve_funcs_span);
        self.logger.ok_or_bail(())
    }

    /// Get the type table (after resolution)
    pub fn into_type_table(self) -> Rc<RefCell<TypeTable>> {
        self.tysys.type_table
    }
}
