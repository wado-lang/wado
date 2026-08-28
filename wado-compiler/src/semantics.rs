//! Semantic analysis — the shared frontend entry point. [`semantics`] runs the
//! same lex → parse → bind → load → analyze → resolve pipeline
//! `compile_with_options` does, then stops: everything downstream exists only to
//! emit Wasm bytes. The resulting [`Semantics`] carries every fact an editor
//! query needs without paying for monomorphize / lower / codegen.

use crate::analyze::Analyzer;
use crate::ast::{AstId, Module};
use crate::ast_index::AstIndex;
use crate::compiler_host::{CompilerHost, LogLevel};
use crate::component_model::CmInterfaceRegistry;
use crate::elaborator::Elaborator;
use crate::elaborator::orchestration::AnnotateState;
use crate::elaborator::sem::{Fact, FactKind, ModuleSemantics};
use crate::hashmap::IndexMap;
use crate::loader;
use crate::logger::Logger;
use crate::module_source::{ModuleSource, ModuleSourceInterner};
use crate::symbol::{Symbol, SymbolTable};
use crate::tir::{ResolvedType, TirModule, TypeId, TypeTable};
use crate::token::Span;
use crate::world_registry::WorldRegistry;

/// A ready-to-query analysis result.
///
/// `Semantics` owns every piece of semantic state produced by the analysis
/// pipeline. The AST modules are preserved verbatim (so positions,
/// [`AstId`]s, and spans resolve against the same tree the parser saw).
/// [`SymbolTable`] is owned; [`TypeTable`] is exposed as an immutable
/// snapshot taken at the end of resolve. LSP queries read the snapshot.
/// The lowering pipeline consumes the shared `state` field to continue
/// interning types into the same table without invalidating the snapshot.
pub struct Semantics {
    pub entry_module_source: ModuleSource,
    pub modules: IndexMap<ModuleSource, Module>,
    pub symbols: SymbolTable,
    pub types: TypeTable,
    /// `ModuleSource` interner shared with the analyze + resolve phases; an LSP
    /// query borrows it to resolve a clicked import path.
    ///
    /// Single-threaded, one `borrow_mut` at a time: never hold a `RefMut` across
    /// a call into [`Semantics`] or [`crate::Elaborator`], or the nested borrow
    /// panics. Write `sem.interner.borrow_mut().<one call>` and let it drop.
    pub interner: std::rc::Rc<std::cell::RefCell<ModuleSourceInterner>>,
    /// Per-module structural index (name spans, write targets, span lookup).
    /// Built once per [`Module`] in [`semantics_of`]. LSP queries (and
    /// the in-tree [`name_span_of`] / [`span_of_key`] helpers) consult this
    /// instead of re-walking the AST on every request.
    pub(crate) ast_indices: IndexMap<ModuleSource, AstIndex>,
    /// Shared elaborator state from [`Elaborator::annotate_modules`], paired with
    /// `is_complete` to distinguish three outcomes: `(None, false)` — analyze or
    /// resolve bailed, leaving only `symbols` + `ast_indices`; `(Some(_), false)`
    /// — annotate finished but `build_tir` bailed; `(Some(_), true)` — full
    /// success. Batch compilation rejects all but the last.
    pub(crate) state: Option<AnnotateState>,
    /// `AstIdSpace → ModuleSource` registry over the loaded modules: which
    /// module's parse minted each id space. Lets bare-`AstId` facts be
    /// resolved back to their owning module (spans, URIs) without carrying a
    /// module in every key.
    pub(crate) space_modules: IndexMap<crate::ast::AstIdSpace, ModuleSource>,
    /// Which module's [`ModuleSemantics`](crate::elaborator::sem::ModuleSemantics)
    /// holds a given fact — the routing every `AstId`-keyed query below goes
    /// through. The facts themselves have one home, the map the walk wrote them
    /// into; this says which one that is.
    ///
    /// A fact lives in the module whose walk recorded it, which is its node's
    /// own module for everything but the foreign AST a walk crosses into (a
    /// callee's parameter default, resolved in the caller's frame). Keyed by
    /// [`FactKind`] as well as node, because one node's kinds need not come
    /// from one walk; where two walks do record the same kind, the last wins.
    ///
    /// The value is a position in [`AnnotateState::module_semantics`], which
    /// the body walk reorders (`swap_remove` + `insert`); it is minted after
    /// the last such move and `state` is immutable from then on. [`Self::fact_at`]
    /// asserts the routed module really holds the fact, so a stale index fails
    /// rather than answering `None`.
    ///
    /// Empty when resolve did not run or bailed before recording any fact.
    pub(crate) fact_home: IndexMap<(AstId, FactKind), u32>,
    /// TIR modules produced by [`crate::elaborator::Elaborator::build_tir_from_state`].
    /// The batch compiler consumes these directly; LSP queries ignore them.
    /// Empty when `build_tir` did not run or bailed.
    pub(crate) tir_modules: IndexMap<ModuleSource, TirModule>,
    /// Source-level liveness produced between `annotate_bodies` and `reify`.
    /// `dead_items` feeds the unused-diagnostics emitter; `emit_live` is the
    /// set reify gates emission on. Empty when annotate did not complete.
    pub(crate) liveness: crate::elaborator::liveness::Liveness,
    /// True when every analysis phase ran to completion without bailing.
    /// Batch compilation refuses to continue when this is false; LSP queries
    /// proceed with whatever partial state the phases managed to produce.
    pub(crate) is_complete: bool,
    /// Compiler-owned WIT emit facts (target world + default interface). Set by
    /// the CLI before WIT emission so `wado wit` and the `wado compile` embed
    /// path derive them identically. `None` until set.
    pub(crate) wit_contract: Option<crate::wit_emit::WitContract>,
}

/// A definition location, assembled from a symbol.
///
/// Returned by [`Semantics::definition_of`]. The `uri` is derived from
/// `ModuleSource::source_path` — it is present for user-authored
/// modules (entry point, local files) and absent for stdlib / builtin
/// sources that have no on-disk URI.
pub struct Definition {
    pub module: ModuleSource,
    pub ast_id: AstId,
    pub span: Option<Span>,
    pub uri: Option<String>,
}

/// Why [`Semantics::resolve_symbol_notation`] could not resolve a notation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolResolveError {
    /// The notation's module is not loaded in this `Semantics`.
    ModuleNotLoaded,
    /// The module is loaded but defines no matching symbol or member.
    SymbolNotFound,
}

impl std::fmt::Display for SymbolResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ModuleNotLoaded => f.write_str("module not found or not loaded"),
            Self::SymbolNotFound => f.write_str("no such symbol in module"),
        }
    }
}

impl std::error::Error for SymbolResolveError {}

impl Semantics {
    /// Power-assert capture plans across every module that annotate reached,
    /// for `wado dump --assert-plan`. `None` before annotate has run.
    pub fn assert_plan_text(&self) -> Option<String> {
        let state = self.state.as_ref()?;
        let mut out = String::new();
        for (module_source, module_sem) in &state.module_semantics {
            let plans = crate::elaborator::assert::render_plans(module_sem);
            if plans.is_empty() {
                continue;
            }
            out.push_str(&format!("// --- Module: {module_source} ---\n"));
            out.push_str(&plans);
        }
        Some(out)
    }

    /// True when every analysis phase ran to completion without bailing.
    ///
    /// Batch compilation should treat `false` here as a hard error
    /// (downstream phases assume populated `state` / `tir_modules`). LSP
    /// queries ignore this flag — they answer whatever partial state allows.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.is_complete
    }

    /// The compiler-owned WIT emit contract, if set by the CLI.
    #[must_use]
    pub fn wit_contract(&self) -> Option<&crate::wit_emit::WitContract> {
        self.wit_contract.as_ref()
    }

    /// Set the WIT emit contract (target world + default interface).
    pub fn set_wit_contract(&mut self, c: crate::wit_emit::WitContract) {
        self.wit_contract = Some(c);
    }

    /// A borrowed [`crate::wit_emit::WitEmitInput`] view over the WIT-relevant
    /// subset.
    #[must_use]
    pub fn wit_emit_input(&self) -> crate::wit_emit::WitEmitInput<'_> {
        crate::wit_emit::WitEmitInput {
            is_complete: self.is_complete,
            tir_modules: &self.tir_modules,
            types: &self.types,
            cm_interface_registry: self.cm_interface_registry(),
            world_registry: self.world_registry(),
            wit_contract: self.wit_contract.as_ref(),
        }
    }

    /// A cloned handle to the CM interface registry `Arc` (`None` before it is
    /// built), for taking a [`crate::wit_emit::WitEmitSnapshot`].
    #[must_use]
    pub fn cm_interface_registry_arc(&self) -> Option<std::sync::Arc<CmInterfaceRegistry>> {
        self.state
            .as_ref()
            .map(|s| std::sync::Arc::clone(&s.tysys.cm_interface_registry))
    }

    /// A cloned handle to the world registry `Arc`, paired with
    /// [`Self::cm_interface_registry_arc`].
    #[must_use]
    pub fn world_registry_arc(&self) -> Option<std::sync::Arc<WorldRegistry>> {
        self.state
            .as_ref()
            .map(|s| std::sync::Arc::clone(&s.world_registry))
    }

    /// Component Model world registry produced during annotate: every `world`
    /// declaration the frontend saw, stdlib and user-declared alike, keyed by
    /// fully-qualified name. Consumed by the WIT producer and by world-shape
    /// decisions in codegen / DCE. `None` when no elaborator state was built —
    /// an LSP query proceeds without world data, batch compilation does not.
    #[must_use]
    pub fn world_registry(&self) -> Option<&WorldRegistry> {
        self.state.as_ref().map(|s| &*s.world_registry)
    }

    /// Component Model interface registry produced during annotate: the resolved
    /// `#[cm(…)]` / `#[cm_import(…)]` view of every CM interface the frontend
    /// saw, powering binding synthesis, lift/lower, and WIT emission. `None`
    /// under the same conditions as [`Self::world_registry`].
    #[must_use]
    /// Name resolution: which declaration a spelling in a module reaches.
    pub(crate) fn resolutions(&self) -> Option<&crate::resolve::Resolutions> {
        self.state.as_ref().map(|s| &*s.tysys.resolutions)
    }

    pub fn cm_interface_registry(&self) -> Option<&CmInterfaceRegistry> {
        self.state.as_ref().map(|s| &*s.tysys.cm_interface_registry)
    }

    /// Construct an empty [`Semantics`] holding only the bookkeeping that
    /// always exists (entry module source, interner) plus per-module
    /// [`AstIndex`] entries for whatever modules the loader returned. Every
    /// downstream field is empty and [`Self::is_complete`] returns `false`.
    ///
    /// Used as the partial-result return value when an analysis phase bails
    /// in [`semantics_of`].
    fn partial(
        entry_module_source: ModuleSource,
        modules: IndexMap<ModuleSource, Module>,
        ast_indices: IndexMap<ModuleSource, AstIndex>,
        symbols: SymbolTable,
        types: TypeTable,
        interner: std::rc::Rc<std::cell::RefCell<ModuleSourceInterner>>,
        state: Option<AnnotateState>,
        fact_home: IndexMap<(AstId, FactKind), u32>,
        tir_modules: IndexMap<ModuleSource, TirModule>,
    ) -> Self {
        let space_modules = modules
            .iter()
            .map(|(ms, m)| (m.ast_id_space(), ms.clone()))
            .collect();
        Self {
            entry_module_source,
            modules,
            symbols,
            types,
            interner,
            ast_indices,
            state,
            space_modules,
            fact_home,
            tir_modules,
            liveness: crate::elaborator::liveness::Liveness::default(),
            is_complete: false,
            wit_contract: None,
        }
    }

    /// Innermost AST node containing the given `(line, column)` in `module`.
    ///
    /// Returns `None` if the module is unknown or no node covers the position.
    /// Answered from the per-module [`AstIndex`];
    /// no AST traversal happens at query time.
    #[must_use]
    pub fn ast_id_at(&self, module: &ModuleSource, line: usize, column: usize) -> Option<AstId> {
        self.ast_indices.get(module)?.ast_id_at(line, column)
    }

    /// The `fact` recorded for `id`, read from the module [`Self::fact_home`]
    /// routes it to. `None` when no walk recorded one.
    fn fact_at<V>(&self, fact: Fact<V>, id: AstId) -> Option<&V> {
        let home = *self.fact_home.get(&(id, fact.kind))? as usize;
        let (_source, sem) = self.state.as_ref()?.module_semantics.get_index(home)?;
        let found = (fact.map)(sem).get(&id);
        debug_assert!(
            found.is_some(),
            "fact_home routed {id:?}/{:?} to a module that does not hold it",
            fact.kind
        );
        found
    }

    /// Every entry of one fact map that [`Self::fact_at`] would answer with —
    /// the iteration side of the same routing, so what an `iter_*` yields is
    /// exactly what a point lookup returns. An entry a later walk shadowed is
    /// left out; the module it lands in is the whole of what makes it live.
    fn iter_live<V>(&self, fact: Fact<V>) -> impl Iterator<Item = (AstId, &V)> {
        self.state
            .as_ref()
            .into_iter()
            .flat_map(|state| state.module_semantics.values().enumerate())
            .flat_map(move |(home, sem)| {
                let home = u32::try_from(home).expect("module count fits in u32");
                (fact.map)(sem)
                    .iter()
                    .filter(move |(id, _)| self.fact_home.get(&(**id, fact.kind)) == Some(&home))
                    .map(|(id, value)| (*id, value))
            })
    }

    /// Symbol for the given key, or `None` if the key does not refer to a
    /// declared symbol. Falls back to the synthetic local table when the key
    /// names a `let` / parameter binding rather than an item.
    #[must_use]
    pub fn symbol_at(&self, id: AstId) -> Option<&Symbol> {
        self.symbols
            .get(&id)
            .or_else(|| self.fact_at(ModuleSemantics::LOCAL_SYMBOLS, id))
    }

    /// Resolve a use-site `AstId` (typically an [`IdentExpr`](crate::ast::IdentExpr) id) to the
    /// `AstId` of its defining binding. Returns `None` if the key does
    /// not appear in the reference map — in which case the caller should
    /// fall back to name-based lookup via the symbol table.
    #[must_use]
    pub fn referenced_symbol(&self, id: AstId) -> Option<AstId> {
        self.fact_at(ModuleSemantics::REFERENCES, id).copied()
    }

    /// Iterate every declared symbol with its canonical key, across all
    /// modules. Includes both item-level symbols and the synthetic local
    /// table (`let` / parameter bindings), mirroring [`Self::symbol_at`].
    /// Callers that want a single module filter on `key.module`. Used by
    /// semantic-token highlighting to classify declaration sites in one pass
    /// instead of a positional lookup per token.
    pub fn iter_symbols(&self) -> impl Iterator<Item = (AstId, &Symbol)> {
        self.symbols
            .iter()
            .map(|(id, s)| (*id, s))
            .chain(self.iter_live(ModuleSemantics::LOCAL_SYMBOLS))
    }

    /// Iterate every recorded use-site `(use_key, def_key)` edge.
    ///
    /// Each `use_key` is typically an [`IdentExpr`](crate::ast::IdentExpr) id; `def_key` is the
    /// binding's defining [`AstId`]. Use sites of locals, parameters,
    /// item-level definitions (functions, types, globals) and imported items
    /// are all recorded here.
    pub fn iter_references(&self) -> impl Iterator<Item = (AstId, AstId)> {
        self.iter_live(ModuleSemantics::REFERENCES)
            .map(|(use_id, def_id)| (use_id, *def_id))
    }

    /// Find every use-site `AstId` whose definition is `def_id`.
    ///
    /// Walks [`Self::iter_references`] and collects matches. The returned keys
    /// can be passed to [`Self::span_of_id`] for source ranges. The defining
    /// occurrence itself is **not** included — callers that want it should add
    /// it via [`Self::name_span_of`] / [`Self::span_of_id`].
    #[must_use]
    pub fn references_to(&self, def_id: AstId) -> Vec<AstId> {
        self.iter_references()
            .filter(|(_use_id, target)| *target == def_id)
            .map(|(use_id, _target)| use_id)
            .collect()
    }

    /// Resolved type for the declaring symbol at `key`, if `key` refers to a
    /// type-declaring AST node (struct, enum, variant, flags, newtype,
    /// resource).
    #[must_use]
    pub fn type_at(&self, id: AstId) -> Option<&ResolvedType> {
        let type_id = self.types.type_of_symbol(&id)?;
        Some(self.types.get(type_id))
    }

    /// Renderable name of the inferred type for a local binding (a let
    /// pattern's `AstId`, a function/closure parameter's `AstId`, or a
    /// `for x of …` element binding's `AstId`). Suitable for inlay-hint
    /// display.
    ///
    /// Returns `None` when `key` does not name a local binding (e.g. it
    /// refers to an item), or when the elaborator bailed before reaching
    /// the binding's body.
    #[must_use]
    pub fn local_type_name(&self, id: AstId) -> Option<String> {
        Some(self.types.type_name(self.local_type(id)?))
    }

    /// Inferred [`TypeId`] for the local binding at `id` (a `let` pattern, a
    /// function / closure parameter, a `for x of …` element), as the body walk
    /// recorded it. `None` for anything that is not a local binding the walk
    /// reached.
    #[must_use]
    pub fn local_type(&self, id: AstId) -> Option<TypeId> {
        self.fact_at(ModuleSemantics::LOCAL_TYPES, id).copied()
    }

    /// Resolved [`TypeId`] for the expression at `key`, recorded by the
    /// elaborator's body walk. Covers every visited [`AstId`] inside a
    /// function body (including operands of binary ops, call arguments,
    /// and trailing block values). Returns `None` when the key does not
    /// name an expression that the elaborator reached — typically an item
    /// id or a body the elaborator bailed on.
    #[must_use]
    pub fn expression_type(&self, id: AstId) -> Option<TypeId> {
        self.fact_at(ModuleSemantics::EXPRESSION_TYPES, id).copied()
    }

    /// Iterate every expression the body walk typed, as `(AstId, TypeId)`.
    /// Pair with [`Self::module_of_id`] to filter by module.
    pub fn iter_expression_types(&self) -> impl Iterator<Item = (AstId, TypeId)> + '_ {
        self.iter_live(ModuleSemantics::EXPRESSION_TYPES)
            .map(|(id, ty)| (id, *ty))
    }

    /// Method-dispatch decision recorded for the `MethodCallExpr` at
    /// `key`, if any. Synthetic calls (for-of's `.into_iter()` /
    /// `.next()`) and the short-circuiting paths inside
    /// `resolve_method_call_with` (tuple `.len()` / `.zip()`,
    /// static-method-as-instance error) leave no entry. See
    /// [`crate::elaborator::sem::types::MethodDispatch`] for the data
    /// shape.
    #[must_use]
    pub(crate) fn method_dispatch_at(
        &self,
        id: AstId,
    ) -> Option<&crate::elaborator::sem::types::MethodDispatch> {
        self.fact_at(ModuleSemantics::METHOD_DISPATCH, id)
    }

    /// Stable public view onto the recorded method-dispatch decision:
    /// `(resolved_function_name, defining_module, self_kind_str)`, or `None` for
    /// a synthetic or short-circuited call path. In-crate consumers read the
    /// full `crate::elaborator::sem::types::MethodDispatch` instead.
    #[must_use]
    pub fn method_dispatch_view(&self, id: AstId) -> Option<(String, ModuleSource, String)> {
        let dispatch = self.method_dispatch_at(id)?;
        let self_kind = match dispatch.self_kind {
            crate::ast::SelfKind::None => "none",
            crate::ast::SelfKind::Value => "value",
            crate::ast::SelfKind::Ref => "ref",
            crate::ast::SelfKind::MutRef => "mut_ref",
        };
        Some((
            dispatch.function_ref.name.clone(),
            dispatch.function_ref.module_source.clone(),
            self_kind.to_string(),
        ))
    }

    /// The `TypeId` of each field of the struct `type_id` names, in declaration
    /// order. `None` if it is not a registered struct or the annotate state is
    /// unavailable. Used by the resource move check's aggregate walk.
    pub(crate) fn struct_field_type_ids_of(&self, type_id: TypeId) -> Option<Vec<TypeId>> {
        self.state.as_ref()?.tysys.struct_field_type_ids_of(type_id)
    }

    /// Whether the method call at `id` takes its receiver `self` by value,
    /// transferring ownership. False for a `&self` / `&mut self` receiver, a
    /// static call, or any call site with no recorded dispatch. The resource
    /// move check uses this to treat the receiver as consumed.
    #[must_use]
    pub fn method_call_consumes_receiver(&self, id: AstId) -> bool {
        self.method_dispatch_at(id)
            .is_some_and(|dispatch| dispatch.consumes_self)
    }

    /// Iterate every recorded method-dispatch decision keyed by the
    /// `MethodCallExpr`'s `(module, AstId)`. Pair with
    /// [`Self::method_dispatch_view`] for the stable public view onto
    /// each entry.
    pub fn iter_method_dispatch(&self) -> impl Iterator<Item = AstId> + '_ {
        self.iter_live(ModuleSemantics::METHOD_DISPATCH)
            .map(|(id, _)| id)
    }

    /// Stable public view onto the recorded coercion choice:
    /// `(coercion_kind_str, target_type_id)` for an expression the body walk
    /// adapted via `try_coerce`, `None` for one that already matched or was
    /// resolved without an expected type. The string is the
    /// `crate::elaborator::sem::types::CoercionKind` variant, snake-cased.
    #[must_use]
    pub fn coercion_view(&self, id: AstId) -> Option<(String, TypeId)> {
        use crate::elaborator::sem::types::CoercionKind;
        let choice = self.fact_at(ModuleSemantics::COERCIONS, id)?;
        let kind = match choice.kind {
            CoercionKind::NumericLiteral => "numeric_literal",
            CoercionKind::NullToOption => "null_to_option",
            CoercionKind::StringNewtype => "string_newtype",
            CoercionKind::BytesNewtype => "bytes_newtype",
            CoercionKind::ClosureToFnNewtype => "closure_to_fn_newtype",
            CoercionKind::TupleToSequence => "tuple_to_sequence",
            CoercionKind::StructToMap => "struct_to_map",
        };
        Some((kind.to_string(), choice.target_type))
    }

    /// Iterate every recorded coercion choice keyed by the source
    /// expression's `(module, AstId)`. Pair with [`Self::coercion_view`]
    /// for the stable public view onto each entry.
    pub fn iter_coercions(&self) -> impl Iterator<Item = AstId> + '_ {
        self.iter_live(ModuleSemantics::COERCIONS).map(|(id, _)| id)
    }

    /// WEP 2026-05-26: stable public view onto the recorded
    /// desugar tag at `key`.
    ///
    /// Returns the variant name (`lower_snake_case`) for a TIR-direct
    /// rewrite site (`assert`, `matches`, comparison chain, for-of,
    /// `while`, compound assignment) or `None` for nodes that did not
    /// take a desugar path. See
    /// `crate::elaborator::sem::types::DesugarKind` for the full
    /// variant set.
    #[must_use]
    pub fn desugar_view(&self, id: AstId) -> Option<String> {
        use crate::elaborator::sem::types::DesugarKind;
        let kind = self.fact_at(ModuleSemantics::DESUGARS, id)?;
        let name = match kind {
            DesugarKind::Assert => "assert",
            DesugarKind::Matches => "matches",
            DesugarKind::ComparisonChain => "comparison_chain",
            DesugarKind::ForOfTuple => "for_of_tuple",
            DesugarKind::ForOfVariadic => "for_of_variadic",
            DesugarKind::ForOfIterator => "for_of_iterator",
            DesugarKind::CStyleFor => "c_style_for",
            DesugarKind::While => "while",
            DesugarKind::WhileLetChain => "while_let_chain",
            DesugarKind::IfLetChain => "if_let_chain",
            DesugarKind::CompoundAssign => "compound_assign",
            DesugarKind::IndexMutMethodCall => "index_mut_method_call",
            DesugarKind::NewtypeFromCollapse => "newtype_from_collapse",
            DesugarKind::NewtypeFromUnwrap => "newtype_from_unwrap",
            DesugarKind::NewtypeFromWrap => "newtype_from_wrap",
        };
        Some(name.to_string())
    }

    /// Iterate every recorded desugar tag keyed by the enclosing AST
    /// node's `(module, AstId)`. Pair with [`Self::desugar_view`] for the
    /// stable public view onto each entry.
    pub fn iter_desugars(&self) -> impl Iterator<Item = AstId> + '_ {
        self.iter_live(ModuleSemantics::DESUGARS).map(|(id, _)| id)
    }

    /// URI (filename) of a module, when the module has one.
    ///
    /// Built-in and stdlib modules have no on-disk URI and return `None`.
    #[must_use]
    pub fn uri_of(&self, module: &ModuleSource) -> Option<String> {
        let uri = module.source_path();
        if uri.is_empty() { None } else { Some(uri) }
    }

    /// AST [`Function`](crate::ast::Function) node declaring `key` — free
    /// functions and `impl` / `trait` methods, which share that AST shape.
    /// `None` for interface and resource methods: they are `Function` nodes
    /// like any other, but the index deliberately skips them, so they are
    /// reached through the symbol table. O(1): the per-module [`AstIndex`]
    /// holds each function's address, so nothing scans the AST at query time.
    #[must_use]
    pub fn function_at(&self, id: AstId) -> Option<&crate::ast::Function> {
        use crate::ast_index::FunctionLocation;
        let owning = self.module_of_id(id)?;
        let module = self.modules.get(owning)?;
        let location = self.ast_indices.get(owning)?.function_location(id)?;
        match location {
            FunctionLocation::Free { item_idx } => match module.items.get(item_idx)? {
                crate::ast::Item::Function(f) => Some(f),
                _ => None,
            },
            FunctionLocation::Method {
                item_idx,
                method_idx,
            } => match module.items.get(item_idx)? {
                crate::ast::Item::Impl(b) => b.methods.get(method_idx),
                crate::ast::Item::Trait(t) => t.methods.get(method_idx),
                _ => None,
            },
        }
    }

    /// Definition location of the symbol identified by `key`.
    ///
    /// Resolves the key to its [`Symbol`], then packages the declaring module,
    /// `AstId`, span, and URI into a [`Definition`]. Returns `None` if the
    /// key does not refer to a declared symbol.
    #[must_use]
    pub fn definition_of(&self, id: AstId) -> Option<Definition> {
        let sym = self.symbol_at(id)?;
        let module = sym.module_source().clone();
        Some(Definition {
            uri: self.uri_of(&module),
            module,
            ast_id: sym.defined_at,
            span: sym.span,
        })
    }

    /// Resolve a parsed [`SymbolNotation`](crate::symbol_notation::SymbolNotation)
    /// to a [`Definition`]. A free notation (`mod#name`) resolves a module-level
    /// symbol; a receiver notation (`mod#Type::m`) a method or associated
    /// constant. The module is resolved against the entry — relative paths
    /// anchor at its directory — and must already be loaded here.
    pub fn resolve_symbol_notation(
        &self,
        notation: &crate::symbol_notation::SymbolNotation,
    ) -> Result<Definition, SymbolResolveError> {
        let module = self.resolve_notation_module(&notation.module);
        if !self.modules.contains_key(&module) {
            return Err(SymbolResolveError::ModuleNotLoaded);
        }
        let Some(receiver) = &notation.receiver else {
            let symbol = self
                .symbols
                .lookup_in_module(&module, &notation.member)
                .ok_or(SymbolResolveError::SymbolNotFound)?;
            let def_module = symbol.module_source().clone();
            let ast_id = symbol.defined_at;
            return Ok(Definition {
                uri: self.uri_of(&def_module),
                module: def_module,
                ast_id,
                span: self.name_span_of(ast_id).or(symbol.span),
            });
        };
        self.resolve_member(&module, receiver, &notation.member)
    }

    /// Resolve `Type::member` / `Type.member` / `Type^Trait::member` against the
    /// `impl` blocks — and, for `Trait::member`, the `trait` declaration — in
    /// `module`. The receiver matches by base name only: `^Trait` disambiguates,
    /// but generic arguments are parsed and ignored, so with both
    /// `impl Foo<i32>` and `impl Foo<bool>` defining `member` the first wins.
    fn resolve_member(
        &self,
        module: &ModuleSource,
        receiver: &crate::symbol_notation::Receiver,
        member: &str,
    ) -> Result<Definition, SymbolResolveError> {
        use crate::ast::Item;
        let ast = self
            .modules
            .get(module)
            .ok_or(SymbolResolveError::ModuleNotLoaded)?;
        let want_type = receiver.base_type_name();
        let want_trait = receiver.base_trait_name();

        for item in &ast.items {
            match item {
                Item::Impl(b) if receiver_matches_impl(b, want_type, want_trait) => {
                    if let Some(m) = b.methods.iter().find(|m| m.name == member) {
                        return Ok(self.member_definition(module, m.id, m.name_span));
                    }
                    if let Some(c) = b.constants.iter().find(|c| c.name == member) {
                        let span = self.name_span_of(c.id).unwrap_or(c.span);
                        return Ok(self.member_definition(module, c.id, span));
                    }
                }
                // `Trait::member` (no `^`) — a method declared on the trait itself.
                Item::Trait(t) if want_trait.is_none() && t.name == want_type => {
                    if let Some(m) = t.methods.iter().find(|m| m.name == member) {
                        return Ok(self.member_definition(module, m.id, m.name_span));
                    }
                }
                _ => {}
            }
        }
        Err(SymbolResolveError::SymbolNotFound)
    }

    /// Names of the members (methods + associated constants) reachable on
    /// `receiver`'s type in `module` — used to suggest targets when a
    /// `Type::member` notation does not resolve. With `public_only`, an
    /// inherent `impl`'s non-`pub` members are omitted (trait-`impl` members
    /// are always included, being the trait's public surface). Sorted and
    /// deduplicated.
    #[must_use]
    pub fn type_member_names(
        &self,
        module: &ModuleSource,
        receiver: &crate::symbol_notation::Receiver,
        public_only: bool,
    ) -> Vec<String> {
        use crate::ast::Item;
        let mut names: Vec<String> = Vec::new();
        let Some(ast) = self.modules.get(module) else {
            return names;
        };
        let want_type = receiver.base_type_name();
        let want_trait = receiver.base_trait_name();
        for item in &ast.items {
            match item {
                Item::Impl(b) if receiver_matches_impl(b, want_type, want_trait) => {
                    let inherent = b.trait_type.is_none();
                    names.extend(
                        b.methods
                            .iter()
                            .filter(|m| member_visible(public_only, inherent, m.visibility))
                            .map(|m| m.name.clone()),
                    );
                    names.extend(
                        b.constants
                            .iter()
                            .filter(|c| member_visible(public_only, inherent, c.visibility))
                            .map(|c| c.name.clone()),
                    );
                }
                Item::Trait(t) if want_trait.is_none() && t.name == want_type => {
                    names.extend(t.methods.iter().map(|m| m.name.clone()));
                }
                _ => {}
            }
        }
        names.sort();
        names.dedup();
        names
    }

    fn member_definition(&self, module: &ModuleSource, id: AstId, name_span: Span) -> Definition {
        Definition {
            uri: self.uri_of(module),
            module: module.clone(),
            ast_id: id,
            span: Some(name_span),
        }
    }

    /// Resolve the module half of a symbol notation to a [`ModuleSource`],
    /// anchored at the entry module (relative paths from its directory;
    /// `core:` / `wasi:` location-independent).
    #[must_use]
    pub fn resolve_notation_module(&self, module_spec: &str) -> ModuleSource {
        let from = self.entry_module_source.clone();
        crate::name::resolve_import_with_entry(
            &mut self.interner.borrow_mut(),
            &from,
            module_spec,
            Some(&from),
        )
    }

    /// Module-level symbol names declared in `module`, sorted and deduplicated.
    /// Used to suggest valid targets when a symbol notation does not resolve.
    /// With `public_only`, only `pub` items (plus `pub use` re-exports) are
    /// listed; otherwise every declared item is.
    #[must_use]
    pub fn module_symbol_names(&self, module: &ModuleSource, public_only: bool) -> Vec<String> {
        use crate::ast::Item;
        let mut names: Vec<String> = Vec::new();
        if let Some(ast) = self.modules.get(module) {
            for item in &ast.items {
                let (visibility, name) = match item {
                    Item::Function(d) => (d.visibility, &d.name),
                    Item::Struct(d) => (d.visibility, &d.name),
                    Item::Enum(d) => (d.visibility, &d.name),
                    Item::Variant(d) => (d.visibility, &d.name),
                    Item::Flags(d) => (d.visibility, &d.name),
                    Item::Newtype(d) => (d.visibility, &d.name),
                    Item::Trait(d) => (d.visibility, &d.name),
                    Item::Resource(d) => (d.visibility, &d.name),
                    Item::Global(d) => (d.visibility, &d.name),
                    Item::Interface(d) => (d.visibility, &d.name),
                    _ => continue,
                };
                if !public_only || visibility.is_public() {
                    names.push(name.clone());
                }
            }
        }
        // `pub use` re-exports are public names regardless of the filter.
        names.extend(self.symbols.reexport_names(module));
        names.sort();
        names.dedup();
        names
    }

    /// Module that owns the node `id`, resolved through the per-parse
    /// [`crate::ast::AstIdSpace`] each loaded module's ids live in. `None`
    /// for ids of unloaded/transient origin (e.g. `AstId::fresh`).
    #[must_use]
    pub fn module_of_id(&self, id: AstId) -> Option<&ModuleSource> {
        self.space_modules.get(&id.space())
    }

    /// Span of the AST node identified by `key` — the source range of the
    /// node itself, not the module's name span. Useful for computing hover /
    /// highlight ranges from use sites. Answered from
    /// [`AstIndex::span_of`](crate::ast_index::AstIndex::span_of), which is
    /// total over every parser-allocated [`AstId`].
    #[must_use]
    pub fn span_of_id(&self, id: AstId) -> Option<Span> {
        self.ast_indices.get(self.module_of_id(id)?)?.span_of(id)
    }

    /// Span of the defining identifier alone — what go-to-definition wants,
    /// where `Symbol::span` covers the whole `fn foo() { … }`. Read from the
    /// per-module [`AstIndex`], which holds one for
    /// every declaration node exposing a `name_span`. `None` for nodes without
    /// one: anonymous `impl` blocks, `Item::Resource`, tests.
    #[must_use]
    pub fn name_span_of(&self, id: AstId) -> Option<Span> {
        self.ast_indices
            .get(self.module_of_id(id)?)?
            .name_span_of(id)
    }

    /// True iff `key` names an `IdentExpr` that appears as the direct
    /// LHS of `=` or a compound-assign in its declaring module. Used by
    /// document-highlight to classify a use-site as Read vs. Write without
    /// re-walking the body.
    #[must_use]
    pub fn is_write_target(&self, id: AstId) -> bool {
        self.module_of_id(id)
            .and_then(|m| self.ast_indices.get(m))
            .is_some_and(|idx| idx.is_write_target(id))
    }

    /// Resolve a 1-based `(line, column)` position to a [`Cursor`] over
    /// `module`. Returns `None` when the module is unknown or no id-bearing
    /// AST node covers the position.
    ///
    /// The returned cursor is *positional* — it always carries the AST id
    /// at the cursor, even if that id is not on a name. Use
    /// [`Cursor::def_key`] to filter to "cursor lands on a recognised
    /// name".
    #[must_use]
    pub fn cursor_at(
        &self,
        module: &ModuleSource,
        line: usize,
        column: usize,
    ) -> Option<Cursor<'_>> {
        let ast_id = self.ast_id_at(module, line, column)?;
        Some(Cursor {
            sem: self,
            module: module.clone(),
            id: ast_id,
        })
    }
}

/// A positional handle into [`Semantics`].
///
/// Captures the cursor's AST id and module so call sites can chain
/// `def_key()`, `def_symbol()`, `references_to_def()`, etc. instead of
/// threading the same `(sem, module, line, col)` tuple through every
/// query helper. Constructed via [`Semantics::cursor_at`].
pub struct Cursor<'a> {
    sem: &'a Semantics,
    module: ModuleSource,
    id: AstId,
}

impl<'a> Cursor<'a> {
    /// Module the cursor is positioned in.
    #[must_use]
    pub fn module(&self) -> &ModuleSource {
        &self.module
    }

    /// `AstId` of the AST node at the cursor.
    #[must_use]
    pub fn key(&self) -> AstId {
        self.id
    }

    /// Source span of the AST node at the cursor, if available.
    #[must_use]
    pub fn span(&self) -> Option<Span> {
        self.sem.span_of_id(self.id)
    }

    /// `AstId` of the binding the cursor names, following the use→def edge
    /// when present. Returns `None` when the cursor does not land on a
    /// recognised name (e.g. on punctuation, on an expression body, on a
    /// numeric literal).
    #[must_use]
    pub fn def_key(&self) -> Option<AstId> {
        if let Some(def) = self.sem.referenced_symbol(self.id) {
            return Some(def);
        }
        if self.sem.symbol_at(self.id).is_some() {
            return Some(self.id);
        }
        None
    }

    /// Symbol named by the cursor, after chasing the use→def edge.
    #[must_use]
    pub fn def_symbol(&self) -> Option<&'a Symbol> {
        self.sem.symbol_at(self.def_key()?)
    }

    /// Identifier-only span of the binding the cursor names (the
    /// `name_span` of the declaration). Returns `None` if the cursor does
    /// not name a known binding or the binding has no narrow name span
    /// (e.g. anonymous `impl` blocks).
    #[must_use]
    pub fn def_name_span(&self) -> Option<Span> {
        self.sem.name_span_of(self.def_key()?)
    }

    /// Best span at the binding's declaration site, falling back from the
    /// narrow `name_span` to the symbol's declared span to the AST node's
    /// span. Returns `None` only when the cursor does not name a known
    /// binding.
    ///
    /// `goto-definition` and `find-references` use this to highlight
    /// declarations that lack a dedicated `name_span` field
    /// (`Item::Resource`, anonymous `impl` blocks, tests).
    #[must_use]
    pub fn def_span(&self) -> Option<Span> {
        let def = self.def_key()?;
        self.sem
            .name_span_of(def)
            .or_else(|| self.sem.symbol_at(def).and_then(|s| s.span))
            .or_else(|| self.sem.span_of_id(def))
    }

    /// True iff the cursor lands on an `IdentExpr` that appears as the
    /// direct LHS of `=` or a compound assignment. Mirrors
    /// [`Semantics::is_write_target`] for the cursor's own key.
    #[must_use]
    pub fn is_write_target(&self) -> bool {
        self.sem.is_write_target(self.id)
    }

    /// Every use-site `AstId` for the binding the cursor names.
    /// Returns an empty `Vec` when the cursor does not name a known binding.
    #[must_use]
    pub fn references_to_def(&self) -> Vec<AstId> {
        match self.def_key() {
            Some(def) => self.sem.references_to(def),
            None => Vec::new(),
        }
    }
}

/// Run the full frontend (parse → load → [`semantics_of`]) over `source` with no
/// kiln invocations and the default log level. Callers needing to inspect the
/// parsed entry between stages, or a custom log level, compose the three
/// primitives instead. Always returns a [`Semantics`]: on failure
/// [`Semantics::is_complete`] is `false` and the downstream fields are empty.
pub async fn semantics<H: CompilerHost>(
    source: &str,
    host: &H,
    filename: Option<&str>,
) -> Semantics {
    semantics_for_world(
        source,
        host,
        filename,
        None,
        crate::kiln::InvocationIndex::new(),
    )
    .await
}

/// [`semantics`] with the target world and kiln redirects threaded through, so a
/// re-analysis off the codegen path matches the main compile.
///
/// `target_world` drives the Kiln `Request<T>` adapter rewrite for the
/// `core:kiln/generator` world; `invocations` redirects `use … with { generator }`
/// clauses to their generated modules. A caller re-analyzing a kiln consumer must
/// pass the same values the compile used, or the load fails and analysis reports
/// incomplete.
pub async fn semantics_for_world<H: CompilerHost>(
    source: &str,
    host: &H,
    filename: Option<&str>,
    target_world: Option<&str>,
    invocations: crate::kiln::InvocationIndex,
) -> Semantics {
    let parsed = crate::parse(source);
    // Surface every recovered lex/parse error, then analyze the partial AST
    // so queries still resolve in the regions outside the error. If load/bind
    // then fails on the partial AST, the result still collapses to
    // `Semantics::empty()` below — recovery helps only when binding succeeds,
    // which holds for the common cases (missing brace, garbage item) where
    // scopes stay separate.
    for e in &parsed.lex_errors {
        host.emit_diagnostic(lex_error_diagnostic(e, filename));
    }
    for e in &parsed.errors {
        host.emit_diagnostic(parse_error_diagnostic(e, filename));
    }
    match crate::load(parsed, filename, host, invocations, LogLevel::default()).await {
        // General entry: build TIR so consumers that read `tir_modules`
        // (kiln options extraction) work. The LSP engine uses its own
        // annotate-only path (`semantics_of(.., build_tir = false)`).
        Ok(mut loaded) => {
            crate::kiln::import_check::inject_kiln_request_adapter(
                target_world,
                &loaded.entry_module_source,
                &mut loaded.modules,
                &mut loaded.cm_source_interfaces,
            );
            semantics_of(loaded, host, LogLevel::default(), true)
        }
        Err(e) => {
            let logger = Logger::new(host, LogLevel::default());
            match filename {
                Some(f) => {
                    let _ = logger.error_at(f, e);
                }
                None => {
                    let _ = logger.error(e);
                }
            }
            Semantics::empty()
        }
    }
}

/// Convert a single recovered [`crate::ParseError`] into a `parse error: …`
/// [`crate::Diagnostic`], attributing the span to `filename`. The
/// error-recovering parser surfaces one of these per syntax error, so the
/// LSP can report them all while still analyzing the partial AST.
#[must_use]
pub fn parse_error_diagnostic(
    err: &crate::ParseError,
    filename: Option<&str>,
) -> crate::Diagnostic {
    use crate::{Code, Diagnostic, DiagnosticSpan, Severity};
    Diagnostic {
        severity: Severity::Error,
        code: Code::InvalidSyntax,
        message: format!("parse error: {}", err.message),
        span: filename.map(|f| DiagnosticSpan {
            file: f.to_string(),
            line: err.span.line,
            column: err.span.column,
            end_line: Some(err.span.end_line),
            end_column: Some(err.span.end_column),
        }),
    }
}

/// Convert a single recovered [`crate::lexer::LexError`] into a
/// `lexer error: …` [`crate::Diagnostic`], attributing the span to
/// `filename`. The resilient lexer surfaces one of these per recovered
/// problem; LSP and batch report them alongside parser diagnostics.
#[must_use]
pub fn lex_error_diagnostic(
    err: &crate::lexer::LexError,
    filename: Option<&str>,
) -> crate::Diagnostic {
    use crate::{Code, Diagnostic, DiagnosticSpan, Severity};
    Diagnostic {
        severity: Severity::Error,
        code: Code::InvalidSyntax,
        message: format!("lexer error: {err}"),
        span: filename.map(|f| DiagnosticSpan {
            file: f.to_string(),
            line: err.span.line,
            column: err.span.column,
            end_line: Some(err.span.end_line),
            end_column: Some(err.span.end_column),
        }),
    }
}

impl Semantics {
    /// An empty [`Semantics`], for when an upstream phase fails outright and
    /// callers still want every query to return its natural empty answer.
    ///
    /// Caveat: `entry_module_source` is [`ModuleSource::entry_point_uninitialized`],
    /// which compares equal to any other `EntryPoint` regardless of filename.
    /// Empty `modules` masks it today; do not populate partial state and rely on
    /// that equality meaning anything.
    #[must_use]
    pub fn empty() -> Self {
        let interner = std::rc::Rc::new(std::cell::RefCell::new(ModuleSourceInterner::new()));
        Semantics::partial(
            ModuleSource::entry_point_uninitialized(),
            IndexMap::default(),
            IndexMap::default(),
            SymbolTable::new(),
            TypeTable::new(),
            interner,
            None,
            IndexMap::default(),
            IndexMap::default(),
        )
    }
}

/// Stage 3 of the frontend: analyze + resolve over an already-loaded module set.
/// Pairs with [`crate::parse`] and [`crate::load`]; [`semantics`] wraps all
/// three. `build_tir` decides whether reify runs — the LSP engine passes `false`
/// since it reads facts and never TIR. Always returns a [`Semantics`]: on bail
/// the downstream fields are empty and [`Semantics::is_complete`] is `false`.
pub fn semantics_of<H: CompilerHost>(
    loaded: loader::LoadResult,
    host: &H,
    log_level: LogLevel,
    build_tir: bool,
) -> Semantics {
    let logger = Logger::new(host, log_level);
    semantics_with_logger(loaded, &logger, build_tir)
}

/// Logger-sharing variant. Internal: lets callers that already maintain a
/// `Logger` for the full compile pipeline nest analyze/resolve trace
/// spans under the same root.
pub(crate) fn semantics_with_logger<H: CompilerHost>(
    load_result: loader::LoadResult,
    logger: &Logger<'_, H>,
    build_tir: bool,
) -> Semantics {
    // Wrap the loader's interner in `Rc<RefCell<>>` so analyze and the
    // per-module elaborators can each `borrow_mut()` it from `&self`
    // contexts. Single-threaded sharing matches the rest of the
    // compiler's `Rc<RefCell<TypeTable>>` plumbing.
    let interner = std::rc::Rc::new(std::cell::RefCell::new(load_result.interner));

    // Build per-module structural indices upfront — they depend only on the
    // parsed AST, so they remain valid even if analyze/resolve bail. This
    // keeps cursor positioning (`Semantics::ast_id_at`) working in partial
    // mode, which the LSP relies on for file-path jumps.
    let ast_indices = {
        let _span = logger.span("ast_index");
        let mut indices: IndexMap<ModuleSource, AstIndex> = IndexMap::default();
        for (source, module) in &load_result.modules {
            indices.insert(source.clone(), AstIndex::build(module));
        }
        indices
    };

    // Per-thread stdlib `Semantics` snapshot.  When present,
    // `annotate_modules` seeds its `TypeTable` / decl maps / registries
    // from this snapshot and `build_tir_from_state` copies the
    // pre-lowered stdlib `TirModule`s directly into its result, skipping
    // the ~28 s of CPU otherwise duplicated across a typical `wado test`
    // run.  Returns `None` when called from inside the snapshot builder
    // itself (re-entry guard); a fresh full pipeline runs in that case.
    let snapshot = {
        let _span = logger.span("stdlib_snapshot");
        crate::stdlib_snapshot::get_or_init_snapshot().filter(|snap| {
            crate::stdlib_snapshot::reparsed_snapshot_module(snap, &load_result.modules).is_none()
        })
    };

    let (symbols, analyze_ok) = {
        let _span = logger.span("analyze");
        let mut analyzer = Analyzer::new(logger)
            .with_invocations(load_result.invocations.clone())
            .with_interner(interner.clone());
        // Bail is converted to `analyze_ok = false`; we still consume the
        // analyzer so partial symbol entries (whatever was recorded before
        // the bail) survive into the returned Semantics.
        let ok = analyzer
            .analyze_loaded_modules(
                &load_result.modules,
                &load_result.entry_module_source,
                load_result.implicit_modules.clone(),
            )
            .is_ok();
        (analyzer.into_symbols(), ok)
    };

    if !analyze_ok {
        return Semantics::partial(
            load_result.entry_module_source,
            load_result.modules,
            ast_indices,
            symbols,
            TypeTable::new(),
            interner,
            None,
            IndexMap::default(),
            IndexMap::default(),
        );
    }

    let included_files = std::rc::Rc::new(load_result.included_files);
    let state = {
        let _span = logger.span("elaborate/annotate");
        Elaborator::annotate_modules(
            &symbols,
            &load_result.modules,
            &load_result.entry_module_source,
            logger,
            included_files,
            load_result.invocations.clone(),
            interner.clone(),
            &load_result.cm_source_interfaces,
            snapshot.as_deref(),
        )
        .ok()
    };
    let Some(mut state) = state else {
        return Semantics::partial(
            load_result.entry_module_source,
            load_result.modules,
            ast_indices,
            symbols,
            TypeTable::new(),
            interner,
            None,
            IndexMap::default(),
            IndexMap::default(),
        );
    };

    // Run the full body-level resolve pass, the single source of truth for
    // use→def edges: LSP and batch compilation both read what the elaborator
    // recorded, with no second lexical scan to drift out of sync. On Bail the
    // partial facts are still routed so cursor queries work against whatever
    // bodies were reached. `build_tir == false` stops after `annotate_bodies`.
    let (tir_modules, lower_ok) = {
        let _span = logger.span("elaborate/build_tir");
        match Elaborator::build_tir_from_state(
            &mut state,
            &symbols,
            &load_result.modules,
            load_result.entry_module_source.clone(),
            logger,
            snapshot.as_deref(),
            build_tir,
        ) {
            Ok(m) => (m, true),
            Err(_) => (IndexMap::default(), false),
        }
    };

    // Take an immutable snapshot of the type table at the end of lowering.
    // LSP queries read this snapshot; any further lowering (none today) would
    // continue interning into the shared `Rc<RefCell<TypeTable>>` held by
    // `state.tysys.type_table`.
    let types = state.tysys.type_table.borrow().clone();

    // Route each fact the body walk recorded to the module that holds it. The
    // `Semantics` API is keyed by bare `AstId` while the facts stay in the
    // per-module `ModuleSemantics` the walk wrote them into — one home,
    // whichever phase reads them. A node reached by two walks (a callee's
    // parameter default, typed at each call site while its own module records
    // the edges around it) routes each kind to the walk that has it.
    let mut fact_home: IndexMap<(AstId, FactKind), u32> = IndexMap::default();
    for (home, sem) in state.module_semantics.values().enumerate() {
        let home = u32::try_from(home).expect("module count fits in u32");
        for fact in sem.routed_facts() {
            fact_home.insert(fact, home);
        }
    }

    // A recovered syntax error anywhere in the loaded set means the parse was
    // partial (covers block-internal errors that leave no `Item::Error` node),
    // so the result is never "complete" even if later phases ran clean.
    let no_syntax_errors = load_result.modules.values().all(|m| !m.has_syntax_errors());

    // Source-level liveness was computed inside `build_tir_from_state`
    // (between `annotate_bodies` and `reify`, so reify can gate on it). Move
    // it onto `Semantics` for the diagnostic emitter and LSP.
    let liveness = std::mem::take(&mut state.liveness);

    let space_modules = load_result
        .modules
        .iter()
        .map(|(ms, m)| (m.ast_id_space(), ms.clone()))
        .collect();
    Semantics {
        entry_module_source: load_result.entry_module_source,
        modules: load_result.modules,
        symbols,
        types,
        interner,
        ast_indices,
        state: Some(state),
        space_modules,
        fact_home,
        tir_modules,
        liveness,
        is_complete: lower_ok && no_syntax_errors,
        wit_contract: None,
    }
}

/// True when `b` is an `impl` on `want_type` and — if `want_trait` is set —
/// implements that trait. Both type names are compared by base name (generic
/// arguments are not matched; see `resolve_member`). Shared by member
/// resolution and the member-suggestion list so they never disagree.
fn receiver_matches_impl(
    b: &crate::ast::ImplBlock,
    want_type: &str,
    want_trait: Option<&str>,
) -> bool {
    if b.ty.head_base_name() != Some(want_type) {
        return false;
    }
    match want_trait {
        Some(wt) => b.trait_type.as_ref().and_then(|t| t.head_base_name()) == Some(wt),
        None => true,
    }
}

/// Whether a type member is shown in the public-API view. Inherent-`impl`
/// members need `pub`; trait-`impl` members are always shown (they are the
/// trait's public surface). Shared with `unparse::unparse_impl_block_signature`.
pub(crate) fn member_visible(
    public_only: bool,
    inherent: bool,
    visibility: crate::ast::Visibility,
) -> bool {
    !public_only || !inherent || visibility.is_public()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compiler_host::InMemoryCompilerHost;

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Runtime::new().unwrap().block_on(future)
    }

    /// One home: the body walk's facts stay in the [`ModuleSemantics`] that
    /// recorded them, and `Semantics` reads through to it. A second copy leaves
    /// the first empty, so a consumer reading the wrong one gets no facts
    /// instead of a failure (WEP 2026-05-26 "One fact, two homes").
    #[test]
    fn body_facts_have_one_home() {
        let host = InMemoryCompilerHost::new();
        let sem = block_on(semantics(
            "fn main() { let x = 1 + 2; let y = x + 3; }",
            &host,
            Some("entry.wado"),
        ));
        let entry = sem.entry_module_source.clone();
        let module_sem = &sem.state.as_ref().expect("annotate state").module_semantics[&entry];

        assert!(!module_sem.types.expression_types.is_empty());
        assert!(!module_sem.types.local_types.is_empty());
        assert!(!module_sem.bindings.references.is_empty());
        assert!(!module_sem.bindings.local_symbols.is_empty());

        let expr_id = *module_sem.types.expression_types.keys().next().unwrap();
        assert!(sem.expression_type(expr_id).is_some());
        let local_id = *module_sem.types.local_types.keys().next().unwrap();
        assert!(sem.local_type_name(local_id).is_some());
        let use_id = *module_sem.bindings.references.keys().next().unwrap();
        assert!(sem.referenced_symbol(use_id).is_some());
        let sym_id = *module_sem.bindings.local_symbols.keys().next().unwrap();
        assert!(sem.symbol_at(sym_id).is_some());

        // The same holds for a module seeded from the stdlib snapshot, which
        // carries its facts per module rather than re-splitting flat ones.
        let stdlib_facts = sem
            .state
            .as_ref()
            .expect("annotate state")
            .module_semantics
            .iter()
            .filter(|(source, _)| **source != entry)
            .any(|(_, sem)| {
                !sem.types.expression_types.is_empty() && !sem.bindings.references.is_empty()
            });
        assert!(stdlib_facts);
    }

    /// What an `iter_*` yields is what a point lookup answers. A node two walks
    /// reached is recorded in both modules' maps; only the one it routes to is
    /// live, so iterating the raw maps would hand out an entry no query returns.
    #[test]
    fn iteration_yields_only_live_facts() {
        let host = InMemoryCompilerHost::new();
        let sem = block_on(semantics(
            "fn main() { let x = 1 + 2; println(`${x}`); }",
            &host,
            Some("entry.wado"),
        ));

        let mut live: crate::hashmap::IndexSet<AstId> = crate::hashmap::IndexSet::default();
        for (id, type_id) in sem.iter_expression_types() {
            assert!(live.insert(id), "iteration yielded {id:?} twice");
            assert_eq!(sem.expression_type(id), Some(type_id));
        }
        let mut seen_uses: crate::hashmap::IndexSet<AstId> = crate::hashmap::IndexSet::default();
        for (use_id, def_id) in sem.iter_references() {
            assert!(
                seen_uses.insert(use_id),
                "iteration yielded {use_id:?} twice"
            );
            assert_eq!(sem.referenced_symbol(use_id), Some(def_id));
        }

        // The dedup is load-bearing here, not vacuous: this program does have
        // nodes two walks reached.
        let recorded: usize = sem
            .state
            .as_ref()
            .expect("annotate state")
            .module_semantics
            .values()
            .map(|module_sem| module_sem.types.expression_types.len())
            .sum();
        assert!(recorded > live.len());
    }
}
