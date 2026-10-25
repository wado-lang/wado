//! Semantic analysis — the shared frontend entry point. [`semantics`] runs the
//! same lex → parse → bind → load → analyze → resolve pipeline
//! `compile_with_options` does, then stops: everything downstream exists only to
//! emit Wasm bytes. The resulting [`Semantics`] carries every fact an editor
//! query needs without paying for monomorphize / lower / codegen.

use std::cell::OnceCell;
use std::sync::Arc;

use crate::analyze::Analyzer;
use crate::ast::{
    AstId, AstIdSpace, AstVisitor, Expr, IdCollector, ImplBlock, Item, Module, SelfKind, Visibility,
};
use crate::ast_index::{AstIndex, FunctionLocation};
use crate::compiler_host::{Code, CompilerHost, LogLevel};
use crate::component_model::{CmInterfaceRegistry, UserCmError, declares_cm_binding};
use crate::coverage::{CoverageMap, CoverageScope, plan_module, render_plan};
use crate::defs::DefId;
use crate::elaborator::Elaborator;
use crate::elaborator::assert::render_plans;
use crate::elaborator::liveness::{Liveness, LivenessScope};
use crate::elaborator::orchestration::{AnnotateState, rt_unreachable_calls};
use crate::elaborator::sem::types::{
    CoercionKind, DesugarKind, FallbackSite, MethodDispatch, TypeAnnotations,
};
use crate::elaborator::sem::{Fact, FactKind, ModuleSemantics};
use crate::hashmap::{IndexMap, IndexSet};
use crate::kiln::InvocationIndex;
use crate::kiln::import_check::inject_kiln_request_adapter;
use crate::logger::Logger;
use crate::marker::write_out_markers;
use crate::module_source::{ModuleSource, ModuleSourceInterner};
use crate::name::resolve_import_with_entry;
use crate::resolve::Resolutions;
use crate::stdlib_snapshot::{get_or_init_snapshot, reparsed_snapshot_module};
use crate::symbol::{Symbol, SymbolTable};
use crate::symbol_notation::SymbolNotation;
use crate::tir::{EffectRows, ResolvedType, TemplateId, TirModule, TypeId};
use crate::token::Span;
use crate::type_table::TypeTable;
use crate::unparse::unparse_contract_clause_renamed;
use crate::wit_emit::{WitContract, WitEmitInput};
use crate::world_registry::{ExportMapping, ExportMappings, WorldRegistry, world_key};
use crate::{ast, load, loader, parse, report_without_span, symbol_notation};

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
    /// Per-module structural index (name spans, write targets, span lookup),
    /// built on the first query into its module ([`Self::ast_index`]). LSP
    /// queries (and the in-tree [`name_span_of`] / [`span_of_key`] helpers)
    /// consult this instead of re-walking the AST on every request. A batch
    /// compile queries few modules, and a package check loads megabytes of
    /// generated parsers none of its queries reach.
    ast_indices: IndexMap<ModuleSource, OnceCell<AstIndex>>,
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
    pub(crate) space_modules: IndexMap<AstIdSpace, ModuleSource>,
    /// The [`ModuleSemantics`] holding each fact the body walk recorded, as a
    /// position in [`AnnotateState::module_semantics`] — the routing every
    /// `AstId`-keyed query below reads through, in place of a second copy.
    /// Keyed by [`FactKind`] too: one node's kinds need not come from one walk.
    /// Several walks can still hold one kind for one node — a callee's
    /// parameter default is typed once per call site — and the last module in
    /// [`AnnotateState::module_semantics`] order is the one routed to.
    pub(crate) fact_home: IndexMap<(AstId, FactKind), u32>,
    /// TIR modules produced by [`crate::elaborator::Elaborator::build_tir_from_state`].
    /// The batch compiler consumes these directly; LSP queries ignore them.
    /// Empty when `build_tir` did not run or bailed.
    pub(crate) tir_modules: IndexMap<ModuleSource, TirModule>,
    /// Source-level liveness produced between `annotate_bodies` and `reify`.
    /// `dead_items` feeds the unused-diagnostics emitter; `emit_live` is the
    /// set reify gates emission on. Empty when annotate did not complete.
    pub(crate) liveness: Liveness,
    /// True when every analysis phase ran to completion without bailing.
    /// Batch compilation refuses to continue when this is false; LSP queries
    /// proceed with whatever partial state the phases managed to produce.
    pub(crate) is_complete: bool,
    /// Compiler-owned WIT emit facts (target world + default interface). Set by
    /// the CLI before WIT emission so `wado wit` and the `wado compile` embed
    /// path derive them identically. `None` until set.
    pub(crate) wit_contract: Option<WitContract>,
    /// What each `export(World::name)` in the entry module names. Empty unless
    /// lowering succeeded.
    pub(crate) export_mappings: ExportMappings,
    /// The name each type the entry's `export use` adopts is published under.
    published_names: IndexMap<DefId, String>,
    /// The coverage plan reify instrumented this compile with, under
    /// `wado test --coverage`.
    pub(crate) coverage: Option<CoverageMap>,
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

/// Where an operand of a call stands among its callee's parameters.
#[derive(Clone, Copy)]
pub(crate) enum Operand {
    Receiver,
    Arg(usize),
}

impl Operand {
    /// The operand where a static call spells the receiver as its first
    /// argument (`Type::method(recv, ..)`).
    fn with_receiver_in_args(self) -> Operand {
        match self {
            Operand::Arg(0) => Operand::Receiver,
            Operand::Arg(i) => Operand::Arg(i - 1),
            Operand::Receiver => unreachable!("such a call spells no receiver"),
        }
    }
}

/// Where a call may put what it was handed: its result, or what a `&mut`
/// operand points to.
#[derive(Clone, Copy)]
pub(crate) enum Destination {
    Result,
    Through(Operand),
}

impl Destination {
    fn with_receiver_in_args(self) -> Destination {
        match self {
            Destination::Result => Destination::Result,
            Destination::Through(to) => Destination::Through(to.with_receiver_in_args()),
        }
    }
}

impl Semantics {
    /// Point every effect variable at the row the effect check solved it to,
    /// in the snapshot and in the table the TIR reads alike.
    pub fn redirect_effect_vars(&mut self, solved: &EffectRows) {
        self.types.redirect_effect_vars(solved);
        if let Some(state) = &self.state {
            state
                .tysys
                .type_table
                .borrow_mut()
                .redirect_effect_vars(solved);
        }
    }

    /// Power-assert capture plans across every module that annotate reached,
    /// for `wado dump --assert-plan`. `None` before annotate has run.
    pub fn assert_plan_text(&self) -> Option<String> {
        let state = self.state.as_ref()?;
        let mut out = String::new();
        for (module_source, module_sem) in &state.module_semantics {
            let plans = render_plans(module_sem);
            if plans.is_empty() {
                continue;
            }
            out.push_str(&format!("// --- Module: {module_source} ---\n"));
            out.push_str(&plans);
        }
        Some(out)
    }

    /// The entry module's coverage plan, for `wado dump --coverage-plan`.
    /// `None` before annotate has run.
    pub fn coverage_plan_text(&self) -> Option<String> {
        let state = self.state.as_ref()?;
        let module = self.modules.get(&self.entry_module_source)?;
        let (plan, ..) = plan_module(
            &self.entry_module_source,
            module,
            &rt_unreachable_calls(state, &self.symbols),
        );
        Some(render_plan(&plan))
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
    pub fn wit_contract(&self) -> Option<&WitContract> {
        self.wit_contract.as_ref()
    }

    /// Set the WIT emit contract (target world + default interface).
    pub fn set_wit_contract(&mut self, c: WitContract) {
        self.wit_contract = Some(c);
    }

    /// A borrowed [`crate::wit_emit::WitEmitInput`] view over the WIT-relevant
    /// subset.
    #[must_use]
    pub fn wit_emit_input(&self) -> WitEmitInput<'_> {
        WitEmitInput {
            is_complete: self.is_complete,
            tir_modules: &self.tir_modules,
            types: &self.types,
            cm_interface_registry: self.cm_interface_registry(),
            world_registry: self.world_registry(),
            wit_contract: self.wit_contract.as_ref(),
            export_mappings: &self.export_mappings,
            published_names: &self.published_names,
        }
    }

    /// The name each type the entry's `export use` adopts is published under.
    #[must_use]
    pub fn published_names(&self) -> &IndexMap<DefId, String> {
        &self.published_names
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

    /// Name resolution: which declaration a spelling in a module reaches.
    #[must_use]
    pub(crate) fn resolutions(&self) -> Option<&Resolutions> {
        self.state.as_ref().map(|s| &*s.tysys.resolutions)
    }

    /// The resolved `#[cm(…)]` / `#[cm_import(…)]` view of every CM interface
    /// the frontend saw. `None` under the same conditions as [`Self::world_registry`].
    #[must_use]
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
        ast_indices: IndexMap<ModuleSource, OnceCell<AstIndex>>,
        symbols: SymbolTable,
        interner: std::rc::Rc<std::cell::RefCell<ModuleSourceInterner>>,
    ) -> Self {
        let space_modules = modules
            .iter()
            .map(|(ms, m)| (m.ast_id_space(), ms.clone()))
            .collect();
        Self {
            entry_module_source,
            modules,
            symbols,
            types: TypeTable::new(),
            interner,
            ast_indices,
            state: None,
            space_modules,
            fact_home: IndexMap::default(),
            tir_modules: IndexMap::default(),
            liveness: Liveness::default(),
            is_complete: false,
            wit_contract: None,
            export_mappings: ExportMappings::default(),
            published_names: IndexMap::default(),
            coverage: None,
        }
    }

    /// Innermost AST node containing the given `(line, column)` in `module`.
    ///
    /// Returns `None` if the module is unknown or no node covers the position.
    /// Answered from the per-module [`AstIndex`]; no AST traversal happens at
    /// query time once the module's index is built.
    #[must_use]
    pub fn ast_id_at(&self, module: &ModuleSource, line: usize, column: usize) -> Option<AstId> {
        self.ast_index(module)?.ast_id_at(line, column)
    }

    fn ast_index(&self, module: &ModuleSource) -> Option<&AstIndex> {
        let cell = self.ast_indices.get(module)?;
        Some(cell.get_or_init(|| AstIndex::build(&self.modules[module])))
    }

    /// The `fact` recorded for `id`, read from the module [`Self::fact_home`]
    /// routes it to. `None` when no walk recorded one; the first walk's where
    /// several did ([`Self::facts_at`]).
    fn fact_at<V>(&self, fact: Fact<V>, id: AstId) -> Option<&V> {
        self.facts_at(fact, id).next()
    }

    /// The patterns naming one float a rounding step can miss, by the module
    /// that writes them, keyed by the span of the arm or statement holding
    /// them. A walk resolving another module's default argument, associated
    /// constant or trait default body notes it in its own facts, so every
    /// walk's notes are gathered and routed by the span's id space.
    pub(crate) fn inexact_float_patterns(
        &self,
    ) -> IndexMap<ModuleSource, IndexMap<Span, IndexSet<String>>> {
        let mut by_module: IndexMap<ModuleSource, IndexMap<Span, IndexSet<String>>> =
            IndexMap::default();
        for types in self.walks() {
            for (site, shown) in &types.inexact_float_patterns {
                let module = self.space_modules[&site.space].clone();
                by_module
                    .entry(module)
                    .or_default()
                    .entry(*site)
                    .or_default()
                    .extend(shown.iter().cloned());
            }
        }
        by_module
    }

    /// The destructuring assignments that keep their desugaring, keyed by the
    /// assignment's span, which names its module. Gathered from every walk, as
    /// [`Self::inexact_float_patterns`] are, so one that falls back in any walk
    /// of a default body is here, whichever module walked it.
    pub(crate) fn destructuring_fallbacks(&self) -> IndexMap<Span, &FallbackSite> {
        self.walks()
            .flat_map(|types| &types.destructuring_fallbacks)
            .map(|(site, fallback)| (*site, fallback))
            .collect()
    }

    /// The facts of every body walk: each module's own, and each trait default
    /// body's, which a walk resolves in the module that takes it.
    fn walks(&self) -> impl Iterator<Item = &TypeAnnotations> {
        self.state.iter().flat_map(|state| {
            state.module_semantics.values().flat_map(|sem| {
                std::iter::once(&sem.types)
                    .chain(sem.default_method_facts.values().map(|facts| &facts.types))
            })
        })
    }

    /// Every value recorded for `fact` at `id`, from the module
    /// [`Self::fact_home`] routes it to: one for a binding, and for a body fact
    /// one per walk that reached the node — the module's own, then each
    /// element's of a tuple `for-of` whose unrolled body holds the node.
    fn facts_at<V>(&self, fact: Fact<V>, id: AstId) -> impl Iterator<Item = &V> {
        let home = self.fact_home.get(&(id, fact.kind)).copied();
        let sem = home
            .and_then(|home| {
                self.state
                    .as_ref()?
                    .module_semantics
                    .get_index(home as usize)
            })
            .map(|(_source, sem)| sem);
        let mut found = sem
            .into_iter()
            .flat_map(move |sem| fact.all(sem, id))
            .peekable();
        debug_assert!(
            home.is_none() || found.peek().is_some(),
            "fact_home routed {id:?}/{:?} to a module that does not hold it",
            fact.kind
        );
        found
    }

    /// Every entry [`Self::fact_at`] would answer with — iteration through the
    /// same routing, so a shadowed entry no query returns is left out, and a
    /// node several walks reached appears once, with the first walk's value.
    fn iter_live<V>(&self, fact: Fact<V>) -> impl Iterator<Item = (AstId, &V)> {
        let mut seen: IndexSet<AstId> = IndexSet::default();
        self.state
            .as_ref()
            .into_iter()
            .flat_map(|state| state.module_semantics.values().enumerate())
            .flat_map(move |(home, sem)| {
                let home = u32::try_from(home).expect("module count fits in u32");
                fact.entries(sem)
                    .filter(move |(id, _)| self.fact_home.get(&(*id, fact.kind)) == Some(&home))
            })
            .filter(move |(id, _)| seen.insert(*id))
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

    /// The symbol a use site at `id` resolves to.
    #[must_use]
    pub(crate) fn referenced_symbol_at(&self, id: AstId) -> Option<&Symbol> {
        self.referenced_symbol(id)
            .and_then(|def| self.symbol_at(def))
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

    /// The type the struct pattern field or case name pattern `id` takes
    /// apart, as the body walk recorded it.
    pub(crate) fn destructured_type(&self, id: AstId) -> Option<TypeId> {
        self.fact_at(ModuleSemantics::DESTRUCTURED_TYPES, id)
            .copied()
    }

    /// [`Self::expression_type`] from every walk that reached the expression:
    /// a tuple `for-of`'s body is walked once per element.
    pub fn expression_types(&self, id: AstId) -> impl Iterator<Item = TypeId> + '_ {
        self.facts_at(ModuleSemantics::EXPRESSION_TYPES, id)
            .copied()
    }

    /// [`Self::local_type`] from every walk that reached the binding.
    pub fn local_types(&self, id: AstId) -> impl Iterator<Item = TypeId> + '_ {
        self.facts_at(ModuleSemantics::LOCAL_TYPES, id).copied()
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
    pub(crate) fn method_dispatch_at(&self, id: AstId) -> Option<&MethodDispatch> {
        self.method_dispatches_at(id).next()
    }

    /// Every dispatch recorded for the method call at `id`: one, or one per
    /// element for a call in the body a tuple `for-of` unrolls, where each
    /// element's receiver may select a different method. A check that must
    /// hold for the call reads them all; [`Self::method_dispatch_at`] answers
    /// the first.
    pub(crate) fn method_dispatches_at(&self, id: AstId) -> impl Iterator<Item = &MethodDispatch> {
        self.facts_at(ModuleSemantics::METHOD_DISPATCH, id)
    }

    /// The parameter types the callee of the call at `id` takes, one list per
    /// walk that resolved it, with this call's type arguments substituted.
    /// A list may lead with the receiver where the call spells it as an
    /// argument, so a reader pairs it with the arguments only where the
    /// lengths agree.
    pub(crate) fn callee_param_types_at(&self, id: AstId) -> impl Iterator<Item = &[TypeId]> {
        let methods = self
            .method_dispatches_at(id)
            .map(|dispatch| dispatch.param_types.as_slice());
        let statics = self
            .facts_at(ModuleSemantics::STATIC_METHOD_DISPATCH, id)
            .map(|dispatch| dispatch.param_types.as_slice());
        let calls = self
            .facts_at(ModuleSemantics::CALL_PARAM_TYPES, id)
            .map(Vec::as_slice);
        methods.chain(statics).chain(calls)
    }

    /// Stable public view onto the recorded method-dispatch decision:
    /// `(resolved_function_name, defining_module, self_kind_str)`, or `None` for
    /// a synthetic or short-circuited call path. In-crate consumers read the
    /// full `crate::elaborator::sem::types::MethodDispatch` instead.
    #[must_use]
    pub fn method_dispatch_view(&self, id: AstId) -> Option<(String, ModuleSource, String)> {
        let dispatch = self.method_dispatch_at(id)?;
        let self_kind = match dispatch.self_kind {
            SelfKind::None => "none",
            SelfKind::Value => "value",
            SelfKind::Ref => "ref",
            SelfKind::MutRef => "mut_ref",
        };
        Some((
            dispatch.function_ref.name.clone(),
            dispatch.function_ref.module_source.clone(),
            self_kind.to_string(),
        ))
    }

    /// Whether a value of `type_id` is move-only in a body whose `affine` type
    /// parameters are `affine_params`.
    pub(crate) fn is_affine(&self, type_id: TypeId, affine_params: &[TypeId]) -> bool {
        self.state
            .as_ref()
            .expect("the move check runs on annotated semantics")
            .tysys
            .is_affine(type_id, affine_params)
    }

    /// The `affine` binders the body of the function declared at `func` sees.
    /// `None` for a body no walk reached, as in the partial semantics the
    /// language service reads after an analysis phase bailed.
    pub(crate) fn affine_params(&self, func: AstId) -> Option<&[TypeId]> {
        let module = self
            .module_of_id(func)
            .expect("a function is declared in a module");
        self.state
            .as_ref()?
            .module_semantics
            .get(module)?
            .types
            .fn_affine_params
            .get(&func)
            .map(Vec::as_slice)
    }

    /// The index of the first field or case of the struct or variant declared
    /// at `decl` that holds an affine value in every instantiation.
    pub(crate) fn first_affine_member(&self, decl: AstId) -> Option<usize> {
        let type_id = self.types.type_of_symbol(&decl)?;
        self.state
            .as_ref()
            .expect("the affine declaration check runs on annotated semantics")
            .tysys
            .first_affine_member(type_id)
    }

    /// Whether a value of `type_id` reaches a `&mut` a call handed it may write
    /// through. Without the annotate state nothing is known, so any may.
    pub(crate) fn reaches_mut_ref(&self, type_id: TypeId) -> bool {
        self.state
            .as_ref()
            .is_none_or(|state| state.tysys.reaches_mut_ref(type_id))
    }

    /// Whether a value of `type_id` may hold a reference.
    pub(crate) fn may_hold_reference(&self, type_id: TypeId) -> bool {
        self.state
            .as_ref()
            .expect("the move check runs on annotated semantics")
            .tysys
            .may_hold_reference(type_id)
    }

    /// Whether a borrow written at `operand` of `call` may reach `destination`,
    /// in any of its recorded dispatches. A callee with no declaration, a
    /// closure or a fn value, may.
    pub(crate) fn borrow_reaches(
        &self,
        call: &Expr,
        operand: Operand,
        destination: Destination,
    ) -> bool {
        let tysys = &self
            .state
            .as_ref()
            .expect("the move check runs on annotated semantics")
            .tysys;
        // `Type::method(..)` and a free function both record a static dispatch;
        // a call of a closure or a fn value records none.
        let callees: Vec<(Option<DefId>, bool)> = match call {
            Expr::MethodCall(mc) => self
                .method_dispatches_at(mc.id)
                .map(|dispatch| (dispatch.method_def, false))
                .collect(),
            Expr::StaticMethodCall(_) | Expr::Call(_) => self
                .facts_at(ModuleSemantics::STATIC_METHOD_DISPATCH, call.id())
                .map(|dispatch| {
                    let declared = dispatch
                        .function_ref
                        .template
                        .as_ref()
                        .and_then(TemplateId::def);
                    (dispatch.method_def.or(declared), dispatch.self_in_args)
                })
                .collect(),
            _ => unreachable!("only a call has operands"),
        };
        callees.is_empty()
            || callees.into_iter().any(|(callee, receiver_in_args)| {
                let (operand, destination) = if receiver_in_args {
                    (
                        operand.with_receiver_in_args(),
                        destination.with_receiver_in_args(),
                    )
                } else {
                    (operand, destination)
                };
                callee.is_none_or(|def| tysys.borrow_reaches(def, operand, destination))
            })
    }

    /// Whether the method call at `id` takes its receiver as `&mut self`, in
    /// any of its recorded dispatches.
    pub(crate) fn method_call_takes_mut_receiver(&self, id: AstId) -> bool {
        self.method_dispatches_at(id)
            .any(|dispatch| dispatch.self_kind == SelfKind::MutRef)
    }

    /// Whether the method call at `id` takes its receiver `self` by value,
    /// transferring ownership — in any of its recorded dispatches. False for
    /// a `&self` / `&mut self` receiver, a static call, or any call site with
    /// no recorded dispatch. The resource move check uses this to treat the
    /// receiver as consumed.
    #[must_use]
    pub fn method_call_consumes_receiver(&self, id: AstId) -> bool {
        self.method_dispatches_at(id)
            .any(|dispatch| dispatch.consumes_self)
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
        let choice = self.fact_at(ModuleSemantics::COERCIONS, id)?;
        let kind = match choice.kind {
            CoercionKind::NumericLiteral => "numeric_literal",
            CoercionKind::NullToOption => "null_to_option",
            CoercionKind::LiteralNewtype => "literal_newtype",
            CoercionKind::BytesNewtype => "bytes_newtype",
            CoercionKind::ClosureToFnNewtype => "closure_to_fn_newtype",
            CoercionKind::CompositeNewtype => "composite_newtype",
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
        let kind = self.fact_at(ModuleSemantics::DESUGARS, id)?;
        let name = match kind {
            DesugarKind::Assert => "assert",
            DesugarKind::Matches => "matches",
            DesugarKind::ComparisonChain => "comparison_chain",
            DesugarKind::ForOfTuple => "for_of_tuple",
            DesugarKind::ForOfVariadic => "for_of_variadic",
            DesugarKind::ForOfIterator => "for_of_iterator",
            DesugarKind::ForOfNever => "for_of_never",
            DesugarKind::CStyleFor => "c_style_for",
            DesugarKind::While => "while",
            DesugarKind::WhileLetChain => "while_let_chain",
            DesugarKind::IfLetChain => "if_let_chain",
            DesugarKind::CompoundAssign => "compound_assign",
            DesugarKind::DestructuringAssign => "destructuring_assign",
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
    pub fn function_at(&self, id: AstId) -> Option<&ast::Function> {
        self.function_and_item_at(id).map(|(f, _)| f)
    }

    /// The function [`Semantics::function_at`] answers for `id`, with the
    /// `contract` clauses that bind it, written as source. A trait method's
    /// live in its trait's hidden holder, and bind every impl of it as well,
    /// read there under the impl's parameter names.
    #[must_use]
    pub fn function_with_contracts_at(&self, id: AstId) -> Option<(&ast::Function, Vec<String>)> {
        let (f, item) = self.function_and_item_at(id)?;
        let (declaring_trait, renames) = if let Item::Impl(b) = item {
            let implemented = self.implemented(b).and_then(Item::as_trait);
            let renames = implemented.map(|t| self.impl_param_renames(t, f));
            (implemented, renames.unwrap_or_default())
        } else {
            (item.as_trait(), IndexMap::default())
        };
        let contracts = f
            .binding_contracts(declaring_trait)
            .into_iter()
            .map(|clause| unparse_contract_clause_renamed(clause, &renames))
            .collect();
        Some((f, contracts))
    }

    /// The clauses [`Semantics::function_with_contracts_at`] answers for `m`,
    /// a method of an analyzed `impl` block.
    #[must_use]
    pub fn method_contracts(&self, m: &ast::Function) -> Vec<String> {
        self.function_with_contracts_at(m.id)
            .expect("an analyzed impl's method is a function")
            .1
    }

    /// Where `t`'s clauses on its method name a parameter that the impl
    /// method `f` names otherwise: each identifier resolving to that
    /// parameter, mapped to `f`'s name for it.
    fn impl_param_renames(&self, t: &ast::TraitDecl, f: &ast::Function) -> IndexMap<AstId, String> {
        let Some(holder) = t.contract_holder(&f.name) else {
            return IndexMap::default();
        };
        let renamed: IndexMap<AstId, &str> = holder
            .params
            .iter()
            .zip(&f.params)
            .filter(|(held, own)| held.name != own.name)
            .map(|(held, own)| (held.id, own.name.as_str()))
            .collect();
        if renamed.is_empty() {
            return IndexMap::default();
        }
        let mut collector = IdCollector::default();
        collector.visit_block(
            holder
                .body
                .as_ref()
                .expect("a holder's body is its clauses"),
        );
        collector
            .ids
            .into_iter()
            .filter_map(|id| {
                let param = self.referenced_symbol(id)?;
                Some((id, (*renamed.get(&param)?).to_owned()))
            })
            .collect()
    }

    /// The `impl` block of `module` spanning `impl_span`. A reader holding its
    /// own parse of the module's source names the block by its span, which
    /// every parse of that source agrees on.
    #[must_use]
    pub fn impl_block_at(&self, module: &ModuleSource, impl_span: Span) -> &ImplBlock {
        self.modules[module]
            .items
            .iter()
            .find_map(|item| {
                let Item::Impl(b) = item else {
                    return None;
                };
                (b.span.start == impl_span.start && b.span.end == impl_span.end).then_some(b)
            })
            .expect("every parse of a module's source spans its impl blocks alike")
    }

    /// The item declaring the trait or interface `b` implements; `None` for an
    /// inherent block.
    #[must_use]
    pub fn implemented(&self, b: &ImplBlock) -> Option<&Item> {
        self.declaration_of(b.trait_type.as_ref()?)
    }

    /// The item declaring the head of `ty`: `Option` for `Option<T>`.
    #[must_use]
    pub fn declaration_of(&self, ty: &ast::Type) -> Option<&Item> {
        let resolutions = self.resolutions()?;
        let id = resolutions.defs().ast_id(resolutions.head_decl(ty)?);
        let module = self.modules.get(self.module_of_id(id)?)?;
        module.items.iter().find(|item| item.id() == id)
    }

    /// The function [`Semantics::function_at`] answers, with the item holding it.
    fn function_and_item_at(&self, id: AstId) -> Option<(&ast::Function, &Item)> {
        let owning = self.module_of_id(id)?;
        let module = self.modules.get(owning)?;
        let location = self.ast_index(owning)?.function_location(id)?;
        let (item_idx, method_idx) = match location {
            FunctionLocation::Free { item_idx } => (item_idx, None),
            FunctionLocation::Method {
                item_idx,
                method_idx,
            } => (item_idx, Some(method_idx)),
        };
        let item = module.items.get(item_idx)?;
        let f = match (item, method_idx) {
            (Item::Function(f), None) => Some(f),
            (Item::Impl(b), Some(i)) => b.methods.get(i),
            (Item::Trait(t), Some(i)) => t.methods.get(i),
            _ => None,
        }?;
        Some((f, item))
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
        notation: &SymbolNotation,
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
        receiver: &symbol_notation::Receiver,
        member: &str,
    ) -> Result<Definition, SymbolResolveError> {
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
        receiver: &symbol_notation::Receiver,
        public_only: bool,
    ) -> Vec<String> {
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
                    names.extend(
                        t.methods
                            .iter()
                            .filter(|m| !m.is_contract_holder())
                            .map(|m| m.name.clone()),
                    );
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
        self.resolve_module_from(&self.entry_module_source, module_spec)
    }

    /// The module `module_spec` names when `from` imports it.
    #[must_use]
    pub fn resolve_module_from(&self, from: &ModuleSource, module_spec: &str) -> ModuleSource {
        resolve_import_with_entry(
            &mut self.interner.borrow_mut(),
            from,
            module_spec,
            Some(&self.entry_module_source),
        )
    }

    /// Module-level symbol names declared in `module`, sorted and deduplicated.
    /// Used to suggest valid targets when a symbol notation does not resolve.
    /// With `public_only`, only `pub` items (plus `pub use` re-exports) are
    /// listed; otherwise every declared item is.
    #[must_use]
    pub fn module_symbol_names(&self, module: &ModuleSource, public_only: bool) -> Vec<String> {
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
        self.ast_index(self.module_of_id(id)?)?.span_of(id)
    }

    /// Span of the defining identifier alone — what go-to-definition wants,
    /// where `Symbol::span` covers the whole `fn foo() { … }`. Read from the
    /// per-module [`AstIndex`], which holds one for
    /// every declaration node exposing a `name_span`. `None` for nodes without
    /// one: anonymous `impl` blocks, `Item::Resource`, tests.
    #[must_use]
    pub fn name_span_of(&self, id: AstId) -> Option<Span> {
        self.ast_index(self.module_of_id(id)?)?.name_span_of(id)
    }

    /// True iff `key` names an `IdentExpr` that appears as the direct
    /// LHS of `=` or a compound-assign in its declaring module. Used by
    /// document-highlight to classify a use-site as Read vs. Write without
    /// re-walking the body.
    #[must_use]
    pub fn is_write_target(&self, id: AstId) -> bool {
        self.module_of_id(id)
            .and_then(|m| self.ast_index(m))
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
    // TIR too, for the consumers that read `tir_modules`.
    semantics_for_world(source, host, filename, None, InvocationIndex::new(), true).await
}

/// [`semantics`] with the target world and kiln redirects threaded through, so a
/// re-analysis off the codegen path matches the main compile.
///
/// `target_world` drives the Kiln `Request<T>` adapter rewrite for the
/// `core:kiln/generator` world; `invocations` redirects `use … with { generator }`
/// clauses to their generated modules. A caller re-analyzing a kiln consumer must
/// pass the same values the compile used, or the load fails and analysis reports
/// incomplete. `build_tir` is as in [`semantics_of`].
pub async fn semantics_for_world<H: CompilerHost>(
    source: &str,
    host: &H,
    filename: Option<&str>,
    target_world: Option<&str>,
    invocations: InvocationIndex,
    build_tir: bool,
) -> Semantics {
    let parsed = parse(source);
    // Surface every recovered lex/parse error, then analyze the partial AST
    // so queries still resolve in the regions outside the error. If load/bind
    // then fails on the partial AST, the result still collapses to
    // `Semantics::empty()` below — recovery helps only when binding succeeds,
    // which holds for the common cases (missing brace, garbage item) where
    // scopes stay separate.
    for e in &parsed.lex_errors {
        host.emit_diagnostic(e.diagnostic(filename));
    }
    for e in &parsed.errors {
        host.emit_diagnostic(e.diagnostic(filename));
    }
    match load(parsed, filename, host, invocations, LogLevel::default()).await {
        Ok(mut loaded) => {
            inject_kiln_request_adapter(
                target_world,
                &loaded.entry_module_source,
                &mut loaded.modules,
                &mut loaded.cm_source_interfaces,
            );
            semantics_of(loaded, host, LogLevel::default(), build_tir)
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
            interner,
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
    semantics_with_logger(
        loaded,
        &logger,
        build_tir,
        None,
        &LivenessScope::Compilation,
    )
}

/// Logger-sharing variant. Internal: lets callers that already maintain a
/// `Logger` for the full compile pipeline nest analyze/resolve trace
/// spans under the same root.
pub(crate) fn semantics_with_logger<H: CompilerHost>(
    mut load_result: loader::LoadResult,
    logger: &Logger<'_, H>,
    build_tir: bool,
    coverage: Option<CoverageScope>,
    liveness_scope: &LivenessScope,
) -> Semantics {
    // Before any phase reports: a diagnostic's span names the file it indexes
    // by looking its parse up here.
    logger.register_parses(
        load_result
            .modules
            .iter()
            .map(|(source, module)| (module.ast_id_space(), source.clone())),
    );

    // Wrap the loader's interner in `Rc<RefCell<>>` so analyze and the
    // per-module elaborators can each `borrow_mut()` it from `&self`
    // contexts. Single-threaded sharing matches the rest of the
    // compiler's `Rc<RefCell<TypeTable>>` plumbing.
    let interner = std::rc::Rc::new(std::cell::RefCell::new(load_result.interner));

    // Per-thread stdlib `Semantics` snapshot.  When present,
    // `annotate_modules` seeds its `TypeTable` / decl maps / registries
    // from this snapshot and `build_tir_from_state` copies the
    // pre-lowered stdlib `TirModule`s directly into its result, skipping
    // the ~28 s of CPU otherwise duplicated across a typical `wado test`
    // run.  Returns `None` when called from inside the snapshot builder
    // itself (re-entry guard); a fresh full pipeline runs in that case.
    let snapshot = {
        let _span = logger.span("stdlib_snapshot");
        // A measured stdlib takes probes the plain snapshot lacks.
        get_or_init_snapshot()
            .filter(|_| !coverage.is_some_and(|scope| scope.stdlib))
            .filter(|snap| reparsed_snapshot_module(snap, &load_result.modules).is_none())
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
    if analyze_ok {
        write_out_markers(&mut load_result.modules, &symbols);
    }

    // Set up once the AST is final, and before anything can bail, so cursor
    // positioning (`Semantics::ast_id_at`) works in partial mode, which the
    // LSP relies on for file-path jumps.
    let ast_indices: IndexMap<ModuleSource, OnceCell<AstIndex>> = load_result
        .modules
        .keys()
        .map(|source| (source.clone(), OnceCell::new()))
        .collect();

    if !analyze_ok {
        return Semantics::partial(
            load_result.entry_module_source,
            load_result.modules,
            ast_indices,
            symbols,
            interner,
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
            interner,
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
            coverage,
            liveness_scope,
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

    // Route each fact to the module that holds it, rather than copying it out:
    // where two walks reached a node, each kind goes to the walk that has it,
    // and a kind both hold to the later module.
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
    let coverage = state.coverage.take();

    let cm_bound = lower_ok && register_user_cm_bindings(&mut state, &load_result.modules, logger);

    let space_modules = load_result
        .modules
        .iter()
        .map(|(ms, m)| (m.ast_id_space(), ms.clone()))
        .collect();
    let export_mappings = if lower_ok {
        export_mappings_of(
            &load_result.modules,
            &load_result.entry_module_source,
            &state.tysys.resolutions,
        )
    } else {
        ExportMappings::default()
    };
    let published_names = state
        .tysys
        .resolutions
        .defs()
        .adopted_by(&load_result.entry_module_source)
        .map(|(def, name)| (def, name.to_string()))
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
        is_complete: cm_bound && no_syntax_errors,
        wit_contract: None,
        export_mappings,
        published_names,
        coverage,
    }
}

/// What each `export(World::name)` in `entry` names, every target having
/// resolved to a world.
fn export_mappings_of(
    modules: &IndexMap<ModuleSource, Module>,
    entry: &ModuleSource,
    resolutions: &Resolutions,
) -> ExportMappings {
    let defs = resolutions.defs();
    let functions = modules[entry].items.iter().filter_map(|item| {
        let Item::Function(func) = item else {
            return None;
        };
        Some(func)
    });
    let mappings = functions
        .flat_map(|func| {
            func.export_targets.iter().map(move |target| {
                let world_def = resolutions
                    .declared(target.id)
                    .expect("lowering rejects an export target naming no world");
                let world_id = defs.ast_id(world_def);
                let world = modules[defs.module(world_def)]
                    .items
                    .iter()
                    .find_map(|item| {
                        let Item::World(world) = item else {
                            return None;
                        };
                        (world.id == world_id).then_some(world)
                    })
                    .expect("a world def is a world declaration in its module");
                ExportMapping {
                    world: world_key(world),
                    export_name: target.name.clone(),
                    function: defs.def_at(func.id),
                    function_name: func.name.clone(),
                }
            })
        })
        .collect();
    ExportMappings::new(mappings)
}

/// Register the CM bindings the program's own modules and its dependencies
/// declare into the analysis' registry. False once one is refused.
fn register_user_cm_bindings<H: CompilerHost>(
    state: &mut AnnotateState,
    modules: &IndexMap<ModuleSource, Module>,
    logger: &Logger<'_, H>,
) -> bool {
    let bindings: Vec<(&ModuleSource, &Module)> = modules
        .iter()
        .filter(|(source, module)| {
            !source.is_core()
                && !source.is_binding()
                && !source.is_wasm_asset()
                && declares_cm_binding(module)
        })
        .collect();
    if bindings.is_empty() {
        return true;
    }
    let refuse = |code: Code, message: String| {
        report_without_span(logger, code, message);
        false
    };
    let tysys = &mut state.tysys;
    let registry = Arc::make_mut(&mut tysys.cm_interface_registry);
    if let Err(error) = registry.register_user_cm_modules(&bindings, &tysys.resolutions) {
        return match error {
            UserCmError::Duplicate(msg) => refuse(Code::DuplicateDefinition, msg),
            UserCmError::UnboundType(msg) => refuse(Code::CmBoundaryType, msg),
        };
    }
    // Only once every module is registered: an `interface` naming a resource's
    // operations may sit in a module other than the one declaring it.
    bindings
        .iter()
        .try_for_each(|(_, module)| registry.validate_cm_function_names(module))
        .map_or_else(|msg| refuse(Code::UnknownType, msg), |()| true)
}

/// True when `b` is an `impl` on `want_type` and — if `want_trait` is set —
/// implements that trait. Both type names are compared by base name (generic
/// arguments are not matched; see `resolve_member`). Shared by member
/// resolution and the member-suggestion list so they never disagree.
fn receiver_matches_impl(b: &ImplBlock, want_type: &str, want_trait: Option<&str>) -> bool {
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
pub(crate) fn member_visible(public_only: bool, inherent: bool, visibility: Visibility) -> bool {
    !public_only || !inherent || visibility.is_public()
}

#[cfg(test)]
mod tests {
    use futures::executor::block_on;

    use super::*;
    use crate::compiler_host::InMemoryCompilerHost;
    use crate::hashmap;

    /// The body walk's facts stay in the [`ModuleSemantics`] that recorded
    /// them, and `Semantics` reads through to it rather than holding a copy.
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

        // The same holds for a module served from the stdlib snapshot, which
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

    /// A stdlib module served from the snapshot reads the snapshot's facts in
    /// place rather than a copy made per compile.
    #[test]
    fn a_compile_shares_the_snapshot_facts() {
        let snap = get_or_init_snapshot().expect("not re-entering the builder");
        let snap_state = snap
            .state
            .as_ref()
            .expect("the stdlib snapshot is complete");
        let host = InMemoryCompilerHost::new();
        let sem = block_on(semantics("fn main() {}", &host, Some("entry.wado")));
        let state = sem.state.as_ref().expect("annotate state");

        let prelude = state
            .module_semantics
            .keys()
            .find(|source| source.to_string() == "core:prelude")
            .expect("every compile loads the prelude");
        assert!(std::rc::Rc::ptr_eq(
            &state.module_semantics[prelude],
            &snap_state.module_semantics[prelude],
        ));
    }

    /// What an `iter_*` yields is what a point lookup answers, so the entry a
    /// node's other walk recorded is not handed out.
    #[test]
    fn iteration_yields_only_live_facts() {
        let host = InMemoryCompilerHost::new();
        let sem = block_on(semantics(
            "fn main() { let x = 1 + 2; println(`${x}`); }",
            &host,
            Some("entry.wado"),
        ));

        let mut live: hashmap::IndexSet<AstId> = hashmap::IndexSet::default();
        for (id, type_id) in sem.iter_expression_types() {
            assert!(live.insert(id), "iteration yielded {id:?} twice");
            assert_eq!(sem.expression_type(id), Some(type_id));
        }
        let mut seen_uses: hashmap::IndexSet<AstId> = hashmap::IndexSet::default();
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
