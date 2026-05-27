//! Semantic analysis — the shared frontend entry point.
//!
//! [`semantics`] drives the compilation pipeline up through name resolution
//! and type resolution (the elaborator's annotate phase), then stops. The
//! resulting [`Semantics`] bundles every semantic fact needed to answer
//! editor queries (hover, go-to-definition, diagnostics) without paying for
//! monomorphize / lower / codegen.
//!
//! The pipeline used here is the same one that `compile_with_options` runs
//! in its early phases: lex → parse → bind → load → analyze → resolve.
//! Everything downstream of resolve is only needed to emit Wasm bytes, so
//! LSP-style consumers skip it.

use crate::analyze::Analyzer;
use crate::ast::{AstId, Module};
use crate::ast_index::AstIndex;
use crate::compiler_host::{CompilerHost, LogLevel};
use crate::elaborator::Elaborator;
use crate::elaborator::orchestration::AnnotateState;
use crate::hashmap::IndexMap;
use crate::loader;
use crate::logger::Logger;
use crate::module_source::{ModuleSource, ModuleSourceInterner};
use crate::symbol::{Symbol, SymbolKey, SymbolTable};
use crate::tir::{ResolvedType, TirModule, TypeId, TypeTable};
use crate::token::Span;

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
    /// `ModuleSource` interner shared with the analyze + resolve phases.
    /// LSP queries (definition / hover / references) borrow this when
    /// they need to resolve an import path the user clicked into a
    /// `ModuleSource`.
    ///
    /// Re-entrancy: only single-threaded callers, and only one
    /// `borrow_mut` at a time. Do not hold a [`std::cell::RefMut`]
    /// across calls into other [`Semantics`] / [`crate::Elaborator`]
    /// methods — a nested `borrow_mut` will panic. The intended
    /// pattern is `sem.interner.borrow_mut().<one method call>`, dropping
    /// the borrow at the statement boundary.
    pub interner: std::rc::Rc<std::cell::RefCell<ModuleSourceInterner>>,
    /// Per-module structural index (name spans, write targets, span lookup).
    /// Built once per [`Module`] in [`semantics_of`]. LSP queries (and
    /// the in-tree [`name_span_of`] / [`span_of_key`] helpers) consult this
    /// instead of re-walking the AST on every request.
    pub(crate) ast_indices: IndexMap<ModuleSource, AstIndex>,
    /// Shared elaborator state produced by [`Elaborator::annotate_modules`].
    ///
    /// `None` when analyze or [`Elaborator::annotate_modules`] bailed before
    /// the state could be built. `Some(_)` once resolve completed — even
    /// if a later [`Elaborator::build_tir_from_state`] bail set
    /// [`Self::is_complete`] to `false`. The pair `(state, is_complete)`
    /// therefore has three distinguishable states:
    ///
    /// - `(None, false)` — analyze or resolve bailed; the snapshot has
    ///   only `symbols` + `ast_indices`.
    /// - `(Some(_), false)` — annotate completed but `build_tir` bailed; the
    ///   snapshot has everything except `tir_modules` (which is empty).
    /// - `(Some(_), true)` — full success.
    ///
    /// Batch compilation rejects every non-`true` case via
    /// [`Semantics::is_complete`], so the `expect` in `compile_with_options`
    /// is safe. LSP queries do not inspect `state` directly.
    pub(crate) state: Option<AnnotateState>,
    /// Use→def map populated by the real elaborator as it walks function
    /// bodies in `build_tir_from_state`. Maps `(module, IdentExpr.id)` to
    /// the binding's defining `SymbolKey`. Empty when resolve did not run
    /// or bailed before recording any edges.
    pub(crate) references: IndexMap<SymbolKey, SymbolKey>,
    /// Local binding [`Symbol`] entries (let / param / closure param)
    /// emitted by the elaborator alongside `references`. Keyed by the
    /// binding's defining [`SymbolKey`]; consulted by
    /// [`Semantics::symbol_at`] when the key does not name an item-level
    /// symbol. Empty when resolve did not run or bailed early.
    pub(crate) locals: IndexMap<SymbolKey, Symbol>,
    /// Inferred [`TypeId`] for each local binding (let / param / closure
    /// param), keyed by the binding's defining [`SymbolKey`]. Populated
    /// alongside [`Self::locals`] from the elaborator. Consumed by LSP
    /// inlay-hint queries via [`Semantics::local_type_name`] to render
    /// the inferred type on bindings without explicit annotation. Empty
    /// when resolve did not run or bailed before recording any bindings.
    pub local_types: IndexMap<SymbolKey, TypeId>,
    /// Resolved [`TypeId`] for every expression visited by the elaborator
    /// body walk, keyed by the expression's `(module, AstId)` pair. The
    /// future `reify` pass (Stage 5 of the elaborator re-architecture WEP)
    /// reads this map to set `TirExpr::type_id` without re-running
    /// inference; LSP hover can consult it for a type at the cursor.
    /// Empty when resolve did not run or bailed before any expression was
    /// visited.
    pub expression_types: IndexMap<SymbolKey, TypeId>,
    /// Method-dispatch decisions recorded for each AST
    /// [`crate::ast::MethodCallExpr`] visited by the body walk, keyed by
    /// the call expression's `(module, AstId)` pair. See
    /// [`crate::elaborator::sem::types::MethodDispatch`] for the contract
    /// (synthetic calls and short-circuiting paths leave no entry).
    pub(crate) method_dispatch:
        IndexMap<SymbolKey, crate::elaborator::sem::types::MethodDispatch>,
    /// Coercion choices recorded for each expression that
    /// [`crate::elaborator::Elaborator::try_coerce`] adapted into its
    /// expected type, keyed by the source expression's `(module, AstId)`.
    /// Expressions that did not need coercion leave no entry. See
    /// [`crate::elaborator::sem::types::CoercionChoice`] for the data
    /// shape.
    pub(crate) coercions: IndexMap<SymbolKey, crate::elaborator::sem::types::CoercionChoice>,
    /// TIR-direct desugar tags recorded by the body walk at each
    /// `assert` / `matches` / comparison-chain / for-of / `while` /
    /// compound-assignment site, keyed by the enclosing AST node's
    /// `(module, AstId)`. See
    /// [`crate::elaborator::sem::types::DesugarKind`].
    pub(crate) desugars: IndexMap<SymbolKey, crate::elaborator::sem::types::DesugarKind>,
    /// TIR modules produced by [`crate::elaborator::Elaborator::build_tir_from_state`].
    /// The batch compiler consumes these directly; LSP queries ignore them.
    /// Empty when `build_tir` did not run or bailed.
    pub(crate) tir_modules: IndexMap<ModuleSource, TirModule>,
    /// True when every analysis phase ran to completion without bailing.
    /// Batch compilation refuses to continue when this is false; LSP queries
    /// proceed with whatever partial state the phases managed to produce.
    pub(crate) is_complete: bool,
}

/// A definition location, assembled from a [`SymbolKey`].
///
/// Returned by [`Semantics::definition_of`]. The `uri` is derived from
/// `ModuleSource::diagnostic_filename` — it is present for user-authored
/// modules (entry point, local files) and absent for stdlib / builtin
/// sources that have no on-disk URI.
pub struct Definition {
    pub module: ModuleSource,
    pub ast_id: AstId,
    pub span: Option<Span>,
    pub uri: Option<String>,
}

impl Semantics {
    /// True when every analysis phase ran to completion without bailing.
    ///
    /// Batch compilation should treat `false` here as a hard error
    /// (downstream phases assume populated `state` / `tir_modules`). LSP
    /// queries ignore this flag — they answer whatever partial state allows.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.is_complete
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
        references: IndexMap<SymbolKey, SymbolKey>,
        locals: IndexMap<SymbolKey, Symbol>,
        local_types: IndexMap<SymbolKey, TypeId>,
        expression_types: IndexMap<SymbolKey, TypeId>,
        method_dispatch: IndexMap<SymbolKey, crate::elaborator::sem::types::MethodDispatch>,
        coercions: IndexMap<SymbolKey, crate::elaborator::sem::types::CoercionChoice>,
        desugars: IndexMap<SymbolKey, crate::elaborator::sem::types::DesugarKind>,
        tir_modules: IndexMap<ModuleSource, TirModule>,
    ) -> Self {
        Self {
            entry_module_source,
            modules,
            symbols,
            types,
            interner,
            ast_indices,
            state,
            references,
            locals,
            local_types,
            expression_types,
            method_dispatch,
            coercions,
            desugars,
            tir_modules,
            is_complete: false,
        }
    }

    /// Innermost AST node containing the given `(line, column)` in `module`.
    ///
    /// Returns `None` if the module is unknown or no node covers the position.
    /// Answered from the per-module [`AstIndex`](crate::ast_index::AstIndex);
    /// no AST traversal happens at query time.
    #[must_use]
    pub fn ast_id_at(&self, module: &ModuleSource, line: usize, column: usize) -> Option<AstId> {
        self.ast_indices.get(module)?.ast_id_at(line, column)
    }

    /// Symbol for the given key, or `None` if the key does not refer to a
    /// declared symbol. Falls back to the synthetic local table when the key
    /// names a `let` / parameter binding rather than an item.
    #[must_use]
    pub fn symbol_at(&self, key: &SymbolKey) -> Option<&Symbol> {
        self.symbols.get(key).or_else(|| self.locals.get(key))
    }

    /// Resolve a use-site `SymbolKey` (typically an [`IdentExpr`] id) to the
    /// `SymbolKey` of its defining binding. Returns `None` if the key does
    /// not appear in the reference map — in which case the caller should
    /// fall back to name-based lookup via the symbol table.
    #[must_use]
    pub fn referenced_symbol(&self, key: &SymbolKey) -> Option<SymbolKey> {
        self.references.get(key).cloned()
    }

    /// Iterate every recorded use-site `(use_key, def_key)` edge.
    ///
    /// Each `use_key` is typically an [`IdentExpr`] id; `def_key` is the
    /// binding's defining [`SymbolKey`]. Use sites of locals, parameters,
    /// item-level definitions (functions, types, globals) and imported items
    /// are all recorded here.
    pub fn iter_references(&self) -> impl Iterator<Item = (&SymbolKey, &SymbolKey)> {
        self.references.iter()
    }

    /// Find every use-site `SymbolKey` whose definition is `def_key`.
    ///
    /// Walks [`Self::iter_references`] and collects matches. The returned keys
    /// can be passed to [`Self::span_of_key`] for source ranges. The defining
    /// occurrence itself is **not** included — callers that want it should add
    /// it via [`Self::name_span_of`] / [`Self::span_of_key`].
    #[must_use]
    pub fn references_to(&self, def_key: &SymbolKey) -> Vec<SymbolKey> {
        self.iter_references()
            .filter(|&(_use_key, target)| target == def_key)
            .map(|(use_key, _target)| use_key.clone())
            .collect()
    }

    /// Resolved type for the declaring symbol at `key`, if `key` refers to a
    /// type-declaring AST node (struct, enum, variant, flags, newtype,
    /// resource).
    #[must_use]
    pub fn type_at(&self, key: &SymbolKey) -> Option<&ResolvedType> {
        let type_id = self.types.type_of_symbol(key)?;
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
    pub fn local_type_name(&self, key: &SymbolKey) -> Option<String> {
        let type_id = self.local_types.get(key).copied()?;
        Some(self.types.type_name(type_id))
    }

    /// Resolved [`TypeId`] for the expression at `key`, recorded by the
    /// elaborator's body walk. Covers every visited [`AstId`] inside a
    /// function body (including operands of binary ops, call arguments,
    /// and trailing block values). Returns `None` when the key does not
    /// name an expression that the elaborator reached — typically an item
    /// id or a body the elaborator bailed on.
    #[must_use]
    pub fn expression_type(&self, key: &SymbolKey) -> Option<TypeId> {
        self.expression_types.get(key).copied()
    }

    /// Method-dispatch decision recorded for the `MethodCallExpr` at
    /// `key`, if any. Synthetic calls (for-of's `.into_iter()` /
    /// `.next()`) and the short-circuiting paths inside
    /// `resolve_method_call_with` (tuple `.len()` / `.zip()`,
    /// static-method-as-instance error) leave no entry. See
    /// [`crate::elaborator::sem::types::MethodDispatch`] for the data
    /// shape.
    ///
    /// `#[allow(dead_code)]` until the consumer (`reify`, Stage 5 of the
    /// WEP) lands.
    #[allow(dead_code)]
    #[must_use]
    pub(crate) fn method_dispatch_at(
        &self,
        key: &SymbolKey,
    ) -> Option<&crate::elaborator::sem::types::MethodDispatch> {
        self.method_dispatch.get(key)
    }

    /// Stage 4 of WEP 2026-05-26: stable public view onto the recorded
    /// method-dispatch decision at `key`.
    ///
    /// Returns `(resolved_function_name, defining_module, self_kind_str)`
    /// for a `MethodCallExpr` whose dispatch was recorded by the body
    /// walk, or `None` for synthetic / short-circuited call paths (see
    /// [`crate::elaborator::sem::types::MethodDispatch`]). Used today as a
    /// reachability probe in tests; the future `reify` pass / LSP hover
    /// path consumes the full [`crate::elaborator::sem::types::MethodDispatch`]
    /// via `pub(crate)` access from inside the crate.
    #[must_use]
    pub fn method_dispatch_view(
        &self,
        key: &SymbolKey,
    ) -> Option<(String, ModuleSource, String)> {
        let dispatch = self.method_dispatch.get(key)?;
        let self_kind = match dispatch.self_kind {
            crate::ast::SelfKind::None => "none",
            crate::ast::SelfKind::Ref => "ref",
            crate::ast::SelfKind::MutRef => "mut_ref",
        };
        Some((
            dispatch.function_ref.name.clone(),
            dispatch.function_ref.module_source.clone(),
            self_kind.to_string(),
        ))
    }

    /// Iterate every recorded method-dispatch decision keyed by the
    /// `MethodCallExpr`'s `(module, AstId)`. Pair with
    /// [`Self::method_dispatch_view`] for the stable public view onto
    /// each entry.
    pub fn iter_method_dispatch(&self) -> impl Iterator<Item = &SymbolKey> {
        self.method_dispatch.keys()
    }

    /// Stage 4 of WEP 2026-05-26: stable public view onto the recorded
    /// coercion choice at `key`.
    ///
    /// Returns `(coercion_kind_str, target_type_id)` for an expression
    /// that the body walk adapted via `try_coerce`, or `None` for
    /// expressions whose resolved type already matched the expected
    /// type (or that were resolved without an expected type). See
    /// [`crate::elaborator::sem::types::CoercionKind`] for the full
    /// variant set; the returned string mirrors the variant name in
    /// lowercase-with-underscores form.
    #[must_use]
    pub fn coercion_view(&self, key: &SymbolKey) -> Option<(String, TypeId)> {
        use crate::elaborator::sem::types::CoercionKind;
        let choice = self.coercions.get(key)?;
        let kind = match choice.kind {
            CoercionKind::NumericLiteral => "numeric_literal",
            CoercionKind::NullToOption => "null_to_option",
            CoercionKind::StringNewtype => "string_newtype",
            CoercionKind::ClosureToFnNewtype => "closure_to_fn_newtype",
            CoercionKind::TupleToSequence => "tuple_to_sequence",
            CoercionKind::StructToMap => "struct_to_map",
        };
        Some((kind.to_string(), choice.target_type))
    }

    /// Iterate every recorded coercion choice keyed by the source
    /// expression's `(module, AstId)`. Pair with [`Self::coercion_view`]
    /// for the stable public view onto each entry.
    pub fn iter_coercions(&self) -> impl Iterator<Item = &SymbolKey> {
        self.coercions.keys()
    }

    /// Stage 4 of WEP 2026-05-26: stable public view onto the recorded
    /// desugar tag at `key`.
    ///
    /// Returns the variant name (lower_snake_case) for a TIR-direct
    /// rewrite site (`assert`, `matches`, comparison chain, for-of,
    /// `while`, compound assignment) or `None` for nodes that did not
    /// take a desugar path. See
    /// [`crate::elaborator::sem::types::DesugarKind`] for the full
    /// variant set.
    #[must_use]
    pub fn desugar_view(&self, key: &SymbolKey) -> Option<String> {
        use crate::elaborator::sem::types::DesugarKind;
        let kind = self.desugars.get(key)?;
        let name = match kind {
            DesugarKind::Assert => "assert",
            DesugarKind::Matches => "matches",
            DesugarKind::ComparisonChain => "comparison_chain",
            DesugarKind::ForOfTuple => "for_of_tuple",
            DesugarKind::ForOfVariadic => "for_of_variadic",
            DesugarKind::ForOfIterator => "for_of_iterator",
            DesugarKind::While => "while",
            DesugarKind::WhileLetChain => "while_let_chain",
            DesugarKind::CompoundAssign => "compound_assign",
        };
        Some(name.to_string())
    }

    /// Iterate every recorded desugar tag keyed by the enclosing AST
    /// node's `(module, AstId)`. Pair with [`Self::desugar_view`] for the
    /// stable public view onto each entry.
    pub fn iter_desugars(&self) -> impl Iterator<Item = &SymbolKey> {
        self.desugars.keys()
    }

    /// URI (filename) of a module, when the module has one.
    ///
    /// Built-in and stdlib modules have no on-disk URI and return `None`.
    #[must_use]
    pub fn uri_of(&self, module: &ModuleSource) -> Option<String> {
        let uri = module.diagnostic_filename();
        if uri.is_empty() { None } else { Some(uri) }
    }

    /// AST [`Function`](crate::ast::Function) node declaring `key`. Covers
    /// top-level free functions and methods inside `Item::Impl` /
    /// `Item::Trait` blocks, all of which share the `Function` AST shape.
    /// Interface and resource methods carry a different AST shape
    /// (`InterfaceMethod`) and are reached through the symbol table
    /// instead — this accessor returns `None` for them.
    ///
    /// Resolution is O(1): the per-module [`AstIndex`] stores each
    /// function's `(item_idx, [method_idx])` address, so no AST scan
    /// happens at query time.
    #[must_use]
    pub fn function_at(&self, key: &SymbolKey) -> Option<&crate::ast::Function> {
        use crate::ast_index::FunctionLocation;
        let module = self.modules.get(&key.module)?;
        let location = self
            .ast_indices
            .get(&key.module)?
            .function_location(key.ast_id)?;
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
    pub fn definition_of(&self, key: &SymbolKey) -> Option<Definition> {
        let sym = self.symbol_at(key)?;
        let defined_at = &sym.defined_at;
        Some(Definition {
            module: defined_at.module.clone(),
            ast_id: defined_at.ast_id,
            span: sym.span,
            uri: self.uri_of(&defined_at.module),
        })
    }

    /// Span of the AST node identified by `key` — the source range of the
    /// node itself, not the module's name span. Useful for computing hover /
    /// highlight ranges from use sites. Answered from
    /// [`AstIndex::span_of`](crate::ast_index::AstIndex::span_of), which is
    /// total over every parser-allocated [`AstId`].
    #[must_use]
    pub fn span_of_key(&self, key: &SymbolKey) -> Option<Span> {
        self.ast_indices.get(&key.module)?.span_of(key.ast_id)
    }

    /// Span of the defining identifier for the symbol at `key`.
    ///
    /// The `Symbol::span` field covers the whole declaring item — e.g. the
    /// entire `fn foo() { ... }` block. LSP go-to-definition wants the
    /// identifier alone (`foo`), which is carried on the AST item as
    /// `name_span`. The lookup goes through the per-module
    /// [`AstIndex`](crate::ast_index::AstIndex), which is populated for every
    /// declaration node that exposes a `name_span` field. Returns `None` for
    /// nodes without a dedicated name span (e.g. anonymous `impl` blocks,
    /// `Item::Resource`, tests).
    #[must_use]
    pub fn name_span_of(&self, key: &SymbolKey) -> Option<Span> {
        self.ast_indices.get(&key.module)?.name_span_of(key.ast_id)
    }

    /// True iff `key` names an `IdentExpr` that appears as the direct
    /// LHS of `=` or a compound-assign in its declaring module. Used by
    /// document-highlight to classify a use-site as Read vs. Write without
    /// re-walking the body.
    #[must_use]
    pub fn is_write_target(&self, key: &SymbolKey) -> bool {
        self.ast_indices
            .get(&key.module)
            .is_some_and(|idx| idx.is_write_target(key.ast_id))
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
            key: SymbolKey::new(module.clone(), ast_id),
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
    key: SymbolKey,
}

impl<'a> Cursor<'a> {
    /// Module the cursor is positioned in.
    #[must_use]
    pub fn module(&self) -> &ModuleSource {
        &self.key.module
    }

    /// `(module, ast_id)` of the AST node at the cursor.
    #[must_use]
    pub fn key(&self) -> &SymbolKey {
        &self.key
    }

    /// Source span of the AST node at the cursor, if available.
    #[must_use]
    pub fn span(&self) -> Option<Span> {
        self.sem.span_of_key(&self.key)
    }

    /// `SymbolKey` of the binding the cursor names, following the use→def
    /// edge when present. Returns `None` when the cursor does not land on a
    /// recognised name (e.g. on punctuation, on an expression body, on a
    /// numeric literal).
    #[must_use]
    pub fn def_key(&self) -> Option<SymbolKey> {
        if let Some(def) = self.sem.referenced_symbol(&self.key) {
            return Some(def);
        }
        if self.sem.symbol_at(&self.key).is_some() {
            return Some(self.key.clone());
        }
        None
    }

    /// Symbol named by the cursor, after chasing the use→def edge.
    #[must_use]
    pub fn def_symbol(&self) -> Option<&'a Symbol> {
        let def_key = self.def_key()?;
        self.sem.symbol_at(&def_key)
    }

    /// Identifier-only span of the binding the cursor names (the
    /// `name_span` of the declaration). Returns `None` if the cursor does
    /// not name a known binding or the binding has no narrow name span
    /// (e.g. anonymous `impl` blocks).
    #[must_use]
    pub fn def_name_span(&self) -> Option<Span> {
        let def_key = self.def_key()?;
        self.sem.name_span_of(&def_key)
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
        let def_key = self.def_key()?;
        self.sem
            .name_span_of(&def_key)
            .or_else(|| self.sem.symbol_at(&def_key).and_then(|s| s.span))
            .or_else(|| self.sem.span_of_key(&def_key))
    }

    /// True iff the cursor lands on an `IdentExpr` that appears as the
    /// direct LHS of `=` or a compound assignment. Mirrors
    /// [`Semantics::is_write_target`] for the cursor's own key.
    #[must_use]
    pub fn is_write_target(&self) -> bool {
        self.sem.is_write_target(&self.key)
    }

    /// Every use-site `SymbolKey` for the binding the cursor names.
    /// Returns an empty `Vec` when the cursor does not name a known binding.
    #[must_use]
    pub fn references_to_def(&self) -> Vec<SymbolKey> {
        match self.def_key() {
            Some(key) => self.sem.references_to(&key),
            None => Vec::new(),
        }
    }
}

/// Convenience: run the full compiler frontend (parse → load →
/// `semantics_of`) over `source` with no kiln invocations and default
/// log level.
///
/// Callers that need to inspect the parsed entry between stages (LSP,
/// kiln-aware drivers) compose the three primitives directly. Callers
/// that need a custom log level or kiln redirects do the same.
///
/// Always returns a [`Semantics`]; on lex/parse/load failure
/// [`Semantics::is_complete`] is `false` and the unreachable downstream
/// fields are empty. Diagnostics are emitted through `host` as the
/// phases run.
pub async fn semantics<H: CompilerHost>(
    source: &str,
    host: &H,
    filename: Option<&str>,
) -> Semantics {
    let parsed = match crate::parse(source) {
        Ok(p) => p,
        Err(e) => {
            host.emit_diagnostic(parse_failure_diagnostic(&e, filename));
            return Semantics::empty();
        }
    };
    match crate::load(
        parsed,
        filename,
        host,
        crate::kiln::InvocationIndex::new(),
        LogLevel::default(),
    )
    .await
    {
        Ok(loaded) => semantics_of(loaded, host, LogLevel::default()),
        Err(e) => {
            let logger = Logger::new(host, LogLevel::default());
            if let Some(f) = filename {
                logger.set_file(f);
            }
            let _ = logger.error(e);
            Semantics::empty()
        }
    }
}

/// Convert a lex/parse failure from [`crate::parse`] into the same
/// [`crate::Diagnostic`] shape the loader emits for
/// `LoadError::{LexError, ParseError}`. Keeps every route to a
/// [`Semantics`] (the [`semantics`] convenience, LSP-side three-stage
/// composition, batch frontend) producing the same wire diagnostic for
/// the same input failure.
#[must_use]
pub fn parse_failure_diagnostic(
    err: &crate::CompileError,
    filename: Option<&str>,
) -> crate::Diagnostic {
    use crate::{Code, Diagnostic, DiagnosticSpan, Severity};
    let (message, line, column) = match err {
        crate::CompileError::Lexer {
            message,
            line,
            column,
            ..
        } => (format!("lexer error: {message}"), *line, *column),
        crate::CompileError::Parser {
            message,
            line,
            column,
            ..
        } => (format!("parse error: {message}"), *line, *column),
        other => (other.to_string(), 1, 1),
    };
    Diagnostic {
        severity: Severity::Error,
        code: Code::InvalidSyntax,
        message,
        span: filename.map(|f| DiagnosticSpan {
            file: f.to_string(),
            line,
            column,
            end_line: None,
            end_column: None,
        }),
    }
}

impl Semantics {
    /// Construct an empty [`Semantics`] with no modules at all. Used when
    /// an upstream phase fails outright (parse error on the entry, load
    /// failure) and callers still want to treat the result uniformly —
    /// every query returns the natural empty answer.
    ///
    /// **Latent caveat**: `entry_module_source` is set to
    /// [`ModuleSource::entry_point_uninitialized`], whose `PartialEq`
    /// equates to any other [`ModuleSource::EntryPoint`] regardless of
    /// filename. Today this is masked because `modules` is empty, so
    /// position lookups bail before reaching helpers that compare the
    /// entry source by equality (e.g. `module_uri`'s
    /// `module == entry` short-circuit). Future changes that populate
    /// partial state into an `empty()` result must not rely on that
    /// equality producing a meaningful distinction.
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
            IndexMap::default(),
            IndexMap::default(),
            IndexMap::default(),
            IndexMap::default(),
            IndexMap::default(),
            IndexMap::default(),
        )
    }
}

/// Stage 3 of the compiler frontend: run analyze + resolve on a pre-loaded
/// module set and return the resulting [`Semantics`].
///
/// Pair with [`crate::parse`] (stage 1) and [`crate::load`] (stage 2). The
/// convenience [`semantics`] wraps all three for callers that don't need
/// to inspect the parsed entry between stages.
///
/// Always returns a [`Semantics`]; on phase bail, downstream fields are
/// empty and [`Semantics::is_complete`] returns `false`.
pub fn semantics_of<H: CompilerHost>(
    loaded: loader::LoadResult,
    host: &H,
    log_level: LogLevel,
) -> Semantics {
    let logger = Logger::new(host, log_level);
    let entry_filename = loaded.entry_module_source.diagnostic_filename();
    if !entry_filename.is_empty() {
        logger.set_file(&entry_filename);
    }
    semantics_with_logger(loaded, &logger)
}

/// Logger-sharing variant. Internal: lets callers that already maintain a
/// `Logger` for the full compile pipeline nest analyze/resolve trace
/// spans under the same root.
pub(crate) fn semantics_with_logger<H: CompilerHost>(
    load_result: loader::LoadResult,
    logger: &Logger<'_, H>,
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
    let snapshot = crate::stdlib_snapshot::get_or_init_snapshot();

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
            IndexMap::default(),
            IndexMap::default(),
            IndexMap::default(),
            IndexMap::default(),
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
            IndexMap::default(),
            IndexMap::default(),
            IndexMap::default(),
            IndexMap::default(),
            IndexMap::default(),
            IndexMap::default(),
        );
    };

    // Run the full body-level resolve pass so each module's
    // `ModuleSemantics::bindings` is populated by the real elaborator. This
    // is the single source of truth for use→def edges — LSP and batch
    // compilation both consume what the elaborator recorded here, with no
    // separate lexical re-scan to drift out of sync.
    //
    // On Bail we still drain the partial reference / local maps so the LSP
    // can answer cursor queries against whatever bodies the elaborator did
    // reach before bailing.
    let (tir_modules, lower_ok) = {
        let _span = logger.span("elaborate/build_tir");
        match Elaborator::build_tir_from_state(
            &mut state,
            &symbols,
            &load_result.modules,
            load_result.entry_module_source.clone(),
            logger,
            snapshot.as_deref(),
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

    // Flatten the per-module `ModuleSemantics` into the maps LSP queries
    // consume directly. The Semantics API stays keyed by `SymbolKey`; the
    // per-module storage on `AnnotateState` is the elaborator-side detail
    // that replaces the previous `Rc<RefCell<…>>` plumbing. The
    // `expression_types` / `method_dispatch` / `coercions` maps are stored
    // per-module keyed by raw `AstId` (the body walk's natural index); the
    // rebind here pairs each id with its owning module so the
    // Semantics-level lookup matches the other flat maps.
    let mut references: IndexMap<SymbolKey, SymbolKey> = IndexMap::default();
    let mut locals: IndexMap<SymbolKey, Symbol> = IndexMap::default();
    let mut local_types: IndexMap<SymbolKey, TypeId> = IndexMap::default();
    let mut expression_types: IndexMap<SymbolKey, TypeId> = IndexMap::default();
    let mut method_dispatch: IndexMap<
        SymbolKey,
        crate::elaborator::sem::types::MethodDispatch,
    > = IndexMap::default();
    let mut coercions: IndexMap<
        SymbolKey,
        crate::elaborator::sem::types::CoercionChoice,
    > = IndexMap::default();
    let mut desugars: IndexMap<
        SymbolKey,
        crate::elaborator::sem::types::DesugarKind,
    > = IndexMap::default();
    for (module_source, sem) in state.module_semantics.iter_mut() {
        references.extend(std::mem::take(&mut sem.bindings.references));
        locals.extend(std::mem::take(&mut sem.bindings.local_symbols));
        local_types.extend(std::mem::take(&mut sem.types.local_types));
        expression_types.extend(
            std::mem::take(&mut sem.types.expression_types)
                .into_iter()
                .map(|(ast_id, type_id)| (SymbolKey::new(module_source.clone(), ast_id), type_id)),
        );
        method_dispatch.extend(
            std::mem::take(&mut sem.types.method_dispatch)
                .into_iter()
                .map(|(ast_id, dispatch)| {
                    (SymbolKey::new(module_source.clone(), ast_id), dispatch)
                }),
        );
        coercions.extend(
            std::mem::take(&mut sem.types.coercions)
                .into_iter()
                .map(|(ast_id, choice)| (SymbolKey::new(module_source.clone(), ast_id), choice)),
        );
        desugars.extend(
            std::mem::take(&mut sem.types.desugars)
                .into_iter()
                .map(|(ast_id, kind)| (SymbolKey::new(module_source.clone(), ast_id), kind)),
        );
    }

    Semantics {
        entry_module_source: load_result.entry_module_source,
        modules: load_result.modules,
        symbols,
        types,
        interner,
        ast_indices,
        state: Some(state),
        references,
        locals,
        local_types,
        expression_types,
        method_dispatch,
        coercions,
        desugars,
        tir_modules,
        is_complete: lower_ok,
    }
}
