//! Reify — AST + [`super::sem::ModuleSemantics`] → [`crate::tir::TirModule`].
//! The mechanical half of the annotate/reify split (WEP 2026-05-26): every
//! TIR-shaping decision is already recorded on `ModuleSemantics`, so this walker
//! only reads them — never re-running inference, resolution, or dispatch. Its
//! `FunctionContext` must land the same locals, at the same indices, annotate did.

use super::sig::AssocConstSig;
use std::cell::RefCell;
use std::rc::Rc;

use crate::ast::{self, AstId, CompoundAssignOp, Expr, Item, Module, UnaryOp};
use crate::compiler_host::CompilerHost;
use crate::hashmap::{IndexMap, IndexSet};
use crate::logger::{Bail, Logger};
use crate::module_source::{ModuleSource, ModuleSourceInterner};
use crate::name::{FqTypeName, Receiver, global_name};
use crate::symbol::SymbolTable;
use crate::tir::{
    self as tir, CallArg, GlobalInit, ResolvedType, TirBinaryOp, TirBlock, TirEnum, TirEnumCase,
    TirExpr, TirExprKind, TirFlags, TirFlagsMember, TirFunction, TirGlobal, TirModule, TirNewtype,
    TirPattern, TirStmt, TirStmtKind, TirStruct, TirTest, TirUnaryOp, TirVariantDecl, TypeId,
    TypeTable,
};

use super::sem::ModuleSemantics;
use super::types::{FunctionContext, TypeLookup};
use super::tysys::TypeSystem;

/// Generate the `ann_*` annotation accessors on [`Reify`], one per
/// [`super::sem::types::BodyFacts`] map from the list `with_body_facts!`
/// names, each reading through [`Reify::ann`]. See the accessor doc comment
/// on the `impl` block for why this exists.
macro_rules! reify_annotation_accessors {
    ($($(#[$doc:meta])* $name:ident => $map:ident : $val:ty),+ $(,)?) => {
        $(
            fn $name(&self, id: crate::ast::AstId) -> Option<$val> {
                self.ann(|facts| &facts.$map, id)
            }
        )+
    };
    // `base { … }`: decl/signature facts that are recorded once per decl and
    // are NOT part of the per-element tuple-for-of overlay (param/return types,
    // type params, effect-op signatures). Same canonical keying, no overlay
    // walk — reify reads them straight from `sem.types`.
    (base { $($name:ident => $map:ident : $val:ty),+ $(,)? }) => {
        $(
            fn $name(&self, id: crate::ast::AstId) -> Option<$val> {
                self.sem.types.$map.get(&id).cloned()
            }
        )+
    };
}

/// Whether a compound-assign target sub-piece may be left inline (duplicated
/// between the read and write) rather than hoisted. It must be side-effect-free
/// AND allocate no reify local: a pure inline piece is walked twice by annotate
/// (read type, then write dispatch) but once by reify, so any local it allocated
/// would desync the frame. Only idents, literals, field access, and deref / ref
/// of a pure operand qualify — notably NOT a list/tuple literal, which
/// materialises a `__b` builder local under sequence coercion.
pub(super) fn ast_expr_is_pure(expr: &ast::Expr) -> bool {
    match expr {
        Expr::Ident(_) | Expr::Literal(_) => true,
        Expr::FieldAccess(f) => ast_expr_is_pure(&f.expr),
        Expr::Unary(u) => {
            matches!(u.op, UnaryOp::Deref | UnaryOp::Ref | UnaryOp::MutRef)
                && ast_expr_is_pure(&u.expr)
        }
        _ => false,
    }
}

/// Whether `expr` is place-shaped — an l-value skeleton (recurse into it to
/// hoist impure subscripts) rather than an owned value like `get_obj()` (bound
/// whole).
fn ast_expr_is_place(expr: &ast::Expr) -> bool {
    match expr {
        Expr::Ident(_) | Expr::FieldAccess(_) | Expr::Index(_) => true,
        Expr::Unary(u) => u.op == UnaryOp::Deref,
        _ => false,
    }
}

/// The side-effecting value operands of a compound-assign target — index
/// subscripts, deref'd references, and owned-value bases (`get_obj().field`) —
/// each bound once so the inline place skeleton is pure to duplicate. Shared by
/// reify (binds `let __caN`, overrides) and annotate (reserves the matching
/// frame slots).
pub(super) fn collect_compound_hoists<'e>(expr: &'e ast::Expr, out: &mut Vec<CompoundHoist<'e>>) {
    match expr {
        Expr::FieldAccess(f) => collect_hoists_of_base(&f.expr, out),
        Expr::Index(ix) => {
            collect_hoists_of_base(&ix.expr, out);
            if !ast_expr_is_pure(&ix.index) {
                out.push(CompoundHoist {
                    piece: &ix.index,
                    index_ctx: Some(ix),
                });
            }
        }
        Expr::Unary(u) if u.op == UnaryOp::Deref => {
            collect_hoists_of_base(&u.expr, out);
        }
        _ => {}
    }
}

/// One side-effecting sub-piece of a compound-assign target to bind once.
pub(super) struct CompoundHoist<'e> {
    pub(super) piece: &'e ast::Expr,
    /// The enclosing index when `piece` is its subscript; drives key-type
    /// coercion on the annotate side.
    pub(super) index_ctx: Option<&'e ast::IndexExpr>,
}

fn collect_hoists_of_base<'e>(base: &'e ast::Expr, out: &mut Vec<CompoundHoist<'e>>) {
    if ast_expr_is_place(base) {
        collect_compound_hoists(base, out);
    } else if !ast_expr_is_pure(base) {
        out.push(CompoundHoist {
            piece: base,
            index_ctx: None,
        });
    }
}

/// One reify-side power-assert capture slot. Independent from
/// [`super::assert::Capture`] so the two walks don't share state.
pub(super) struct ReifyAssertSlot {
    /// `__v0`, `__v1`, … — the local the panic template references.
    pub(super) name: String,
    pub(super) label: String,
    /// `false` when the sub-expression evaporated during reify and no
    /// binding was emitted; the template skips the slot.
    pub(super) emitted: bool,
    pub(super) local_index: Option<u32>,
    pub(super) type_id: Option<crate::tir::TypeId>,
    /// See [`super::sem::types::AssertSlot::conditional`].
    pub(super) conditional: bool,
    /// See [`super::sem::types::AssertSlot::is_place`].
    pub(super) is_place: bool,
    /// See [`super::sem::types::AssertSlot::hoisted`].
    pub(super) hoisted: bool,
    /// The operand itself, for a slot the failure branch re-reads.
    pub(super) place_expr: Option<TirExpr>,
    /// The `bool` recording whether the capture site ran.
    pub(super) seen_local_index: Option<u32>,
}

/// The callee a literal coercion names, as annotate resolved and mangled it
/// (WEP 2026-08-24).
fn literal_callee_ref(callee: &super::sem::types::LiteralCallee) -> crate::tir::FunctionRef {
    use crate::tir::{FunctionRef, MonomorphInfo};

    FunctionRef {
        module_source: callee.impl_module_source.clone(),
        name: callee.mangled_name.clone(),
        monomorph_info: (!callee.type_arg_ids.is_empty()).then(|| MonomorphInfo {
            generic_name: format!("{}::{}", callee.target_base_name, callee.method),
            impl_type_args: callee.type_arg_ids.clone(),
            method_type_args: vec![],
            is_blanket: false,
        }),
        method_info: Some(
            crate::name::LocalMethodName::new(
                callee.target_base_name.clone(),
                Some(callee.trait_name.clone()),
                callee.method.to_string(),
            )
            .with_struct_type_args(&callee.type_arg_names),
        ),
    }
}

/// `Output::from(array)` for a literal coercion (WEP 2026-08-24). An `Array<E>`
/// target needs no conversion — the array the literal materializes is already
/// the result — and is returned unchanged.
fn build_literal_from_call(
    array: TirExpr,
    call: &super::sem::types::LiteralFromCall,
    span: crate::token::Span,
) -> TirExpr {
    if call.from_type == call.output_type {
        return array;
    }
    TirExpr::new(
        crate::tir::TirExprKind::Call {
            func: Box::new(literal_callee_ref(&call.callee)),
            type_args: vec![],
            args: vec![crate::tir::CallArg::new(array, false)],
            has_receiver: false,
        },
        call.output_type,
        span,
    )
}

/// Cast a `from` result to the newtype the literal targeted, where it targeted
/// one.
fn cast_to_newtype(
    built: TirExpr,
    newtype_cast_to: Option<crate::tir::TypeId>,
    span: crate::token::Span,
) -> TirExpr {
    match newtype_cast_to {
        Some(target_type) => TirExpr::new(
            crate::tir::TirExprKind::Cast {
                expr: Box::new(built),
                target_type,
            },
            target_type,
            span,
        ),
        None => built,
    }
}

/// `Output::from([[k0, v0], …])` over one run of a key-value literal's explicit
/// members.
fn build_kv_from_call(
    pairs: Vec<TirExpr>,
    facts: &super::sem::types::KeyValueCoercionFacts,
    span: crate::token::Span,
) -> TirExpr {
    let array = TirExpr::new(
        crate::tir::TirExprKind::ArrayLiteral { elements: pairs },
        facts.call.from_type,
        span,
    );
    build_literal_from_call(array, &facts.call, span)
}

/// `__acc.spread_literal(base)` for one `..base` member.
fn build_literal_spread_call(
    acc_index: u32,
    output_type: crate::tir::TypeId,
    base: TirExpr,
    spread: &super::sem::types::LiteralCallee,
    span: crate::token::Span,
) -> TirExpr {
    let receiver = TirExpr::new(
        crate::tir::TirExprKind::Local {
            index: acc_index,
            name: "__acc".to_string(),
        },
        output_type,
        span,
    );
    TirExpr::new(
        crate::tir::TirExprKind::method_call(
            Box::new(receiver),
            literal_callee_ref(spread),
            vec![],
            vec![crate::tir::CallArg::new(base, false)],
        ),
        crate::tir::TypeTable::UNIT,
        span,
    )
}

/// `target = value;` as a statement.
fn assign_stmt(target: TirExpr, value: TirExpr, span: crate::token::Span) -> TirStmt {
    let type_id = value.type_id;
    TirStmt::new(
        TirStmtKind::Expr(TirExpr::new(
            crate::tir::TirExprKind::Assign {
                target: Box::new(target),
                value: Box::new(value),
            },
            type_id,
            span,
        )),
        span,
    )
}

pub(super) struct ReifyAssertCaptureContext {
    pub(super) slots: Vec<ReifyAssertSlot>,
    pub(super) ast_id_to_slot: IndexMap<AstId, usize>,
    /// Guard so the `reify_expr` hook doesn't re-fire on the same
    /// `AstId` during its own recursive reify call.
    pub(super) in_progress: IndexSet<AstId>,
    pub(super) emitted_lets: Vec<TirStmt>,
}

/// Per-module reify pass: emits a [`TirModule`] from the AST plus the
/// `ModuleSemantics` that `annotate` populated. One instance per module the
/// batch driver emits TIR for; constructed via [`Reify::new`].
pub(crate) struct Reify<'a, H: CompilerHost> {
    /// Pipeline-wide type knowledge. `&mut` only because reify may
    /// intern new monomorphic instances; the trait/impl tables are
    /// treated as read-only per the WEP `Reify surface` contract.
    pub(crate) tysys: TypeSystem,
    /// Per-module semantic facts produced by `annotate_bodies`. Read
    /// only — reify never mutates the recorded decisions.
    pub(crate) sem: &'a ModuleSemantics,
    /// All modules' semantics, keyed by source. Used to swap `sem` to a
    /// callee module when reifying a default-argument expression that
    /// resolves in the callee's lexical scope (it may reference items
    /// private to the callee module).
    pub(crate) all_module_semantics: &'a IndexMap<ModuleSource, ModuleSemantics>,
    /// Symbol table from analyzer (cross-module).
    pub(crate) symbols: &'a SymbolTable,
    /// All loaded modules. Used by cross-module lookups (e.g. resolving
    /// the AST of a function referenced by a `FunctionRef`).
    pub(crate) loaded_modules: &'a IndexMap<ModuleSource, Module>,
    /// Diagnostics logger.
    pub(crate) logger: &'a Logger<'a, H>,
    /// Source module currently being reified.
    pub(crate) current_module_source: ModuleSource,
    /// Items of the current module, set before per-Item dispatch.
    pub(crate) current_module_items: &'a [Item],
    /// `ModuleSource` interner. Shared with annotate so cross-pass
    /// references resolve to the same `ModuleSource` identity.
    pub(crate) interner: Rc<RefCell<ModuleSourceInterner>>,
    /// Type-parameter names in scope for the function/method body
    /// currently being reified (impl params first, then method-level
    /// params, matching the index layout reify builds in
    /// `reify_method` / `reify_function`). Empty outside a body walk.
    /// `resolve_type` consults this so a turbofish type argument naming
    /// an enclosing type param (`v.serialize::<S>(s)` inside a generic
    /// method) resolves to its `TypeParam` slot instead of `unknown`.
    pub(crate) current_type_param_names: Vec<String>,
    /// Names of the effect parameters (`<effect E>`) in scope for the
    /// function / method currently being reified. `reify_effects` and
    /// `apply_function_type_effects` consult this so an effect name that is a
    /// param resolves to [`crate::tir::EffectRef::Param`] rather than a
    /// `Concrete` effect — matching `Elaborator::resolve_effects`. Without
    /// it a `fn(...) with E` parameter type would carry `Concrete { E }`,
    /// which fails to unify with the enclosing function's recorded
    /// `Param { E }` declared effect at indirect-call effect checks.
    pub(crate) current_effect_param_names: Vec<String>,
    /// Active per-element annotation overlays for the tuple `for-of`(s)
    /// currently being unrolled, innermost last. While reifying element
    /// `i` of a tuple for-of, that element's [`super::sem::types::BodyFacts`]
    /// sits on top; [`Self::ann`] consults the stack from the top down before
    /// falling back to `sem.types`. A nested inner for-of pushes its own
    /// overlay above the outer one, so inner-body nodes shadow correctly while
    /// outer-body nodes fall through to the outer overlay. See
    /// [`Self::reify_tuple_for_of`].
    pub(crate) tuple_overlay_stack: Vec<&'a super::sem::types::BodyFacts>,
    /// Per-`ForOfStmt` visit counter. Annotate records one overlay set per
    /// *instantiation* of a tuple for-of in walk order; reify increments
    /// this each time it reifies the same `for_of.id` so it consumes the
    /// matching instantiation (a nested inner for-of is instantiated once
    /// per outer element). See [`Self::reify_tuple_for_of`].
    pub(crate) tuple_overlay_visits: IndexMap<crate::ast::AstId, usize>,
    /// Source-level emit set. When `Some`, reify skips a function or method
    /// whose `AstId` is absent — one the liveness pass found the emitted
    /// program cannot reach, which downstream phases would discard anyway.
    /// `None` reifies everything.
    pub(crate) emit_live: Option<&'a IndexSet<crate::ast::AstId>>,
    /// Active parameter-name → already-reified-argument substitutions for the
    /// default-argument expression being reified. A default resolves under the
    /// *callee's* perspective, but a reference to an earlier parameter is the
    /// *caller's* argument, already reified under the caller's — so `reify_ident`
    /// returns the pre-reified TIR instead of re-resolving the spliced AST.
    pub(crate) default_arg_overrides: IndexMap<String, TirExpr>,
    /// Active `AstId` → already-reified-`Local` substitutions for the
    /// side-effecting sub-pieces of a compound-assign target, bound once to
    /// `let __caN` before the read/write. Reify returns the bound `Local` for
    /// those exact nodes so `arr[bump()] += 1` evaluates `bump()` once while
    /// the place skeleton (index calls, field / deref chain) stays inline and
    /// still writes back. Empty outside `reify_compound_assign`.
    pub(crate) compound_overrides: IndexMap<crate::ast::AstId, TirExpr>,
    /// Call site for location literals (`#file` / `#line` / `#function`) in a
    /// default-argument expression, which report the call site rather than the
    /// callee module reify swaps to for name resolution. Set only by the
    /// outermost default walk (nested defaulted calls inherit it); `None`
    /// elsewhere. See [`Self::reify_pad_args_with_defaults`].
    pub(crate) call_site_location: Option<CallSiteLocation>,
    /// Local struct declarations (`Stmt::Item`) built while walking a
    /// function body's statements, flushed into the module's TIR at the
    /// same point `pending_anonymous_structs` is. Reify-owned (not on
    /// `sem.decls`, which is `&`-only from here) because a local item has
    /// no `Item::Struct` entry in `module.items` for the per-item dispatch
    /// loop to walk — `reify_stmt`'s `Stmt::Item` arm is the only place
    /// that discovers it. See `reify_local_item`.
    pub(crate) pending_local_structs: Vec<TirStruct>,
    /// Local newtype declarations (`Stmt::Item`) — same reasoning as
    /// `pending_local_structs`.
    pub(crate) pending_local_newtypes: Vec<TirNewtype>,
}

/// Call site captured for location literals in defaults.
/// See [`Reify::call_site_location`].
#[derive(Clone)]
pub(crate) struct CallSiteLocation {
    pub(crate) module: ModuleSource,
    pub(crate) span: crate::token::Span,
    pub(crate) function_name: String,
}

impl<'a, H: CompilerHost> Reify<'a, H> {
    /// The symbol `name` reaches from `module` — see
    /// [`super::Elaborator::symbol_named`], which answers the same way from the
    /// same tables, so annotate and reify cannot disagree about what a name
    /// means.
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

    /// The declaration a qualified path's *owner* segment names — see
    /// `Elaborator::qualified_owner_decl`, which answers the same way from the
    /// same table.
    fn qualified_owner_decl(&self, ident: &ast::IdentExpr) -> Option<crate::defs::DefId> {
        let owner = ident.segments.len().checked_sub(2)?;
        self.tysys.resolutions.declared(ident.segments[owner].id)
    }

    /// The reference site of a qualified path's *owner* segment — `Color` in
    /// `Color::Red`, `Color` in `ns::Color::Red`. `None` for a bare name, which
    /// qualifies nothing.
    fn qualified_owner_site(&self, ident: &ast::IdentExpr) -> Option<crate::ast::AstId> {
        let owner = ident.segments.len().checked_sub(2)?;
        Some(ident.segments[owner].id)
    }

    /// A `Type::Case` identifier as the declaration owning the case and the
    /// spelling: `Color::Red` at its own segments, a bare `Red` as annotate
    /// read it off the expected type.
    fn case_path(&self, ident: &ast::IdentExpr) -> Option<(Option<crate::defs::DefId>, String)> {
        if let Some(owner) = self.ann_bare_case(ident.id) {
            return Some((Some(owner), self.tysys.qualified_case(owner, &ident.name)));
        }
        let (prefix, _) = ident.name.split_once("::")?;
        let owner = self
            .type_lookup()
            .declaration_at(self.qualified_owner_site(ident), prefix);
        Some((owner, ident.name.clone()))
    }

    /// The symbol row behind a reference site — see
    /// `Elaborator::symbol_at`, which answers the same way from the same
    /// table, so annotate and reify cannot disagree.
    fn symbol_at(&self, site: crate::ast::AstId) -> Option<&'a crate::symbol::Symbol> {
        let def = self.tysys.resolutions.declared_if_walked(site)?;
        self.symbols.get(&self.tysys.resolutions.defs().ast_id(def))
    }

    /// The impl-associated constant `owner` declares as `name` — the same
    /// answer `Elaborator::associated_constant_of` gives, from the same table,
    /// so annotate and reify cannot disagree about which constant a use site
    /// names.
    fn associated_constant_of(
        &self,
        owner: crate::defs::DefId,
        name: &str,
    ) -> Option<super::sig::AssocConstSig> {
        self.tysys
            .signatures
            .associated_constant(owner, name)
            .cloned()
    }

    /// [`Self::associated_constant_of`] for a qualified path in expression
    /// position, whose leading segment carries the site that names the owner.
    fn associated_constant_of_path(
        &self,
        ident: &ast::IdentExpr,
    ) -> Option<super::sig::AssocConstSig> {
        let owner = super::trait_query::assoc_const_owner_of_path(ident, &self.tysys.resolutions)?;
        let name = ident.segments.last()?;
        self.associated_constant_of(owner, &name.name)
    }

    /// [`Self::associated_constant_of`] for a pattern's `Type::CONST`
    /// spelling, whose qualifier is a written `ast::Type` with its own site.
    fn associated_constant_qualified(
        &self,
        qualifier: Option<&ast::Type>,
        name: &str,
    ) -> Option<super::sig::AssocConstSig> {
        let owner = super::trait_query::assoc_const_owner(qualifier, &self.tysys.resolutions)?;
        self.associated_constant_of(owner, name)
    }

    /// Construct a per-module `Reify` for the orchestration driver. The `tysys`
    /// clone is the shallow one [`TypeSystem`] supports. `current_module_source`
    /// / `current_module_items` are placeholders here — [`Self::reify_module`]
    /// overwrites them; keeping them on the struct saves threading them through
    /// every method signature.
    pub(crate) fn new(
        tysys: TypeSystem,
        sem: &'a ModuleSemantics,
        all_module_semantics: &'a IndexMap<ModuleSource, ModuleSemantics>,
        symbols: &'a SymbolTable,
        loaded_modules: &'a IndexMap<ModuleSource, Module>,
        logger: &'a Logger<'a, H>,
        interner: Rc<RefCell<ModuleSourceInterner>>,
        emit_live: Option<&'a IndexSet<crate::ast::AstId>>,
    ) -> Self {
        Self {
            tysys,
            sem,
            all_module_semantics,
            symbols,
            loaded_modules,
            logger,
            current_module_source: ModuleSource::entry_point_uninitialized(),
            current_module_items: &[],
            interner,
            current_type_param_names: Vec::new(),
            current_effect_param_names: Vec::new(),
            tuple_overlay_stack: Vec::new(),
            tuple_overlay_visits: IndexMap::default(),
            emit_live,
            default_arg_overrides: IndexMap::default(),
            compound_overrides: IndexMap::default(),
            call_site_location: None,
            pending_local_structs: Vec::new(),
            pending_local_newtypes: Vec::new(),
        }
    }

    /// The body fact `map` records for `id` in the walk reify is replaying:
    /// the active tuple `for-of` overlays innermost-first, then the module's
    /// own walk. Every read of a body fact goes through here, because annotate
    /// peeled each element's facts out of the module's own maps.
    fn ann<V: Clone>(
        &self,
        map: fn(&super::sem::types::BodyFacts) -> &IndexMap<crate::ast::AstId, V>,
        id: crate::ast::AstId,
    ) -> Option<V> {
        self.tuple_overlay_stack
            .iter()
            .rev()
            .copied()
            .chain(std::iter::once(&self.sem.types.body))
            .find_map(|facts| map(facts).get(&id).cloned())
    }

    super::sem::types::with_body_facts!(reify_annotation_accessors);

    /// Recorded type of an expression, reporting an indefinite one as absent
    /// so the node falls back to its `expected_type`.
    ///
    /// The body walk records indefinite types for its own AST analyses;
    /// building with one reifies a bare `null` as an `Option` nothing inhabits
    /// and fails WIR validation.
    fn ann_expression_types(&self, id: crate::ast::AstId) -> Option<crate::tir::TypeId> {
        let raw = self.ann_recorded_expression_type(id)?;
        (!self.tysys.type_table.borrow().is_indefinite(raw)).then_some(raw)
    }

    // Decl/signature facts the body walk records once per decl, read
    // straight from `sem.types` with no overlay walk. A pack-spread subject
    // joins them: its `(name, slot)` names the enclosing signature's pack, so
    // it is the same for every element a tuple `for-of` unrolls.
    reify_annotation_accessors! {
        base {
            ann_pack_spread_subject => pack_spread_subjects: (String, u32),
            ann_method_impl_type_params => method_impl_type_params: Vec<crate::tir::TirTypeParam>,
            ann_fn_param_types => fn_param_types: Vec<crate::tir::TypeId>,
            ann_fn_return_type => fn_return_types: crate::tir::TypeId,
            ann_effect_ops => effect_ops: Vec<crate::tir::TirEffectOp>,
            ann_decl_type_params => decl_type_params: Vec<crate::tir::TirTypeParam>,
            ann_struct_field_types => struct_field_types: Vec<crate::tir::TypeId>,
            ann_method_names => method_names: super::sem::types::MethodNames,
        }
    }

    /// Build a [`TypeLookup`] view over the current module's import
    /// context and the shared `all_*` tables. Used by `reify_*` helpers
    /// that need to resolve AST `Type` nodes (e.g. type-param defaults,
    /// resource method param/return types) the same way the elaborator
    /// did during annotate — but without the elaborator's
    /// `record_type_name_reference` side-effect (use→def edges were
    /// already recorded by annotate and live on
    /// [`ModuleSemantics::bindings`]).
    fn type_lookup(&self) -> TypeLookup<'_> {
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
            // A function-local item is reached through the `local_*` tables
            // above, keyed by declaration — not the per-function tier below,
            // which annotate clears and reify never repopulates.
            fn_local_items: &self.sem.decls.fn_local_items,
            decls: Some(&self.tysys.trait_env),
        }
    }

    /// Resolve an effect-name list into [`crate::tir::EffectRef`]s
    /// for a function signature. Mirrors
    /// [`super::Elaborator::resolve_effects`] without the use→def
    /// recording side-effect (annotate already recorded the edges).
    fn reify_effects(&self, effects: &[String]) -> Vec<crate::tir::EffectRef> {
        effects
            .iter()
            .map(|name| {
                // Effect params in scope (`<effect E>`) become `Param`, matching
                // `Elaborator::resolve_effects`; otherwise they would resolve to
                // a `Concrete` effect and fail to unify with the recorded
                // `Param` declared effect at effect checks.
                if self.current_effect_param_names.iter().any(|p| p == name) {
                    crate::tir::EffectRef::Param { name: name.clone() }
                } else if let Some(source) = self.sem.imports.effect_sources.get(name).cloned() {
                    let canonical = self
                        .symbols
                        .lookup_in_module(&source, name)
                        .map(|sym| sym.module_source().clone())
                        .unwrap_or_else(|| source.clone());
                    crate::tir::EffectRef::Concrete {
                        name: name.clone(),
                        module_source: canonical,
                    }
                } else {
                    let canonical = self
                        .symbol_named(&self.current_module_source, name)
                        .map(|sym| sym.module_source().clone())
                        .unwrap_or_else(|| self.current_module_source.clone());
                    crate::tir::EffectRef::Concrete {
                        name: name.clone(),
                        module_source: canonical,
                    }
                }
            })
            .collect()
    }

    /// Resolve an AST [`ast::Type`] to a [`TypeId`] without recording
    /// any use→def edge. Reify uses this for type-level resolutions
    /// (type-param defaults, resource method params, …) — annotate
    /// already recorded the edges during its body walk.
    ///
    /// Delegates to the existing
    /// [`super::Elaborator::resolve_type_static`] helper, which is
    /// host-agnostic and operates over the [`TypeLookup`] view above.
    fn resolve_type(&mut self, ty: &ast::Type) -> TypeId {
        let lookup = self.type_lookup();
        // Resolve within the current body's type-parameter scope so a
        // turbofish argument that names an enclosing type param resolves
        // to its `TypeParam` slot. Outside a body walk the scope is empty,
        // so this is identical to the scope-free path.
        let resolved = super::Elaborator::<H>::resolve_type_static_with_params(
            ty,
            &mut self.tysys.type_table.borrow_mut(),
            &lookup,
            &self.current_type_param_names,
        );
        self.apply_function_type_effects(ty, resolved)
    }

    /// Re-intern a resolved `fn(...) with E` type carrying its effects: the
    /// shared static resolver has no effect-resolution context and leaves
    /// `effects` empty, so a `fn`-typed parameter loses its `with` clause.
    /// the effect check then can't see that, e.g., `f: fn() with Stdout`
    /// requires `Stdout` at an indirect call site. Resolves effects through
    /// the same [`Self::reify_effects`] used for declared effects (so the
    /// `EffectRef`s stay canonically consistent across the module). Handles a
    /// bare `Function` and one behind `&` / `&mut`.
    fn apply_function_type_effects(&self, ty: &ast::Type, resolved: TypeId) -> TypeId {
        use crate::tir::ResolvedType;
        match ty {
            ast::Type::Reference(inner) => {
                let pointee = match self.tysys.type_table.borrow().get(resolved) {
                    ResolvedType::Ref(p) => *p,
                    _ => return resolved,
                };
                let fixed = self.apply_function_type_effects(inner, pointee);
                if fixed == pointee {
                    resolved
                } else {
                    self.tysys.type_table.borrow_mut().make_ref(fixed)
                }
            }
            ast::Type::MutReference(inner) => {
                let pointee = match self.tysys.type_table.borrow().get(resolved) {
                    ResolvedType::MutRef(p) => *p,
                    _ => return resolved,
                };
                let fixed = self.apply_function_type_effects(inner, pointee);
                if fixed == pointee {
                    resolved
                } else {
                    self.tysys.type_table.borrow_mut().make_mut_ref(fixed)
                }
            }
            ast::Type::Function(ft) if !ft.effects.is_empty() => {
                let effects = self.reify_effects(&ft.effects);
                let rebuilt = match self.tysys.type_table.borrow().get(resolved) {
                    ResolvedType::Function {
                        is_mut,
                        params,
                        return_type,
                        stores,
                        ..
                    } => ResolvedType::Function {
                        is_mut: *is_mut,
                        params: params.clone(),
                        return_type: *return_type,
                        effects,
                        stores: stores.clone(),
                    },
                    _ => return resolved,
                };
                self.tysys.type_table.borrow_mut().intern(rebuilt)
            }
            _ => resolved,
        }
    }

    /// The identity of the declaration at `id`, which the emitted `TirFunction`
    /// carries so a later pass can ask what it was reified from.
    fn def_of(&self, id: crate::ast::AstId) -> Option<crate::defs::DefId> {
        self.tysys.resolutions.defs().of_ast_id(id)
    }

    /// True when liveness gating is active and nothing the emitted program
    /// keeps reaches `id`, so it never reaches monomorphization.
    fn is_dead_item(&self, id: crate::ast::AstId) -> bool {
        self.emit_live.is_some_and(|live| !live.contains(&id))
    }

    pub(crate) fn reify_module(
        &mut self,
        module: &'a Module,
        module_source: ModuleSource,
    ) -> Result<TirModule, Bail> {
        self.current_module_source = module_source.clone();
        self.current_module_items = &module.items;

        let mut tir_module = TirModule::new(module_source);

        for item in &module.items {
            match item {
                Item::Function(func) => {
                    // A bodyless function is a declaration — a builtin or an
                    // import. There is no body to skip emitting, and the call
                    // that names it may be one synthesis mints later.
                    if func.body.is_some() && self.is_dead_item(func.id) {
                        continue;
                    }
                    if let Some(tir_func) = self.reify_function(func) {
                        tir_module.add_function(tir_func);
                    }
                }
                Item::Struct(struct_decl) => {
                    tir_module.add_struct(self.reify_struct(struct_decl));
                }
                Item::Impl(impl_block) => {
                    if let Some(tir_impl) = self.reify_impl_decl(impl_block) {
                        tir_module.add_impl(tir_impl);
                    }
                    for tir_func in self.reify_impl(impl_block) {
                        tir_module.add_function(tir_func);
                    }
                    // Reify is the sole producer of
                    // trait default-method `TirFunction`s, synthesised here
                    // from the per-impl `ModuleSemantics` snapshots the
                    // body walk recorded on `sem.default_method_semantics`.
                    for tir_func in self.reify_impl_default_methods(impl_block) {
                        tir_module.add_function(tir_func);
                    }
                }
                Item::Trait(_) => {
                    // Trait declarations don't lower to TIR; the elaborator
                    // already registered the signature on `TraitEnv`.
                }
                Item::Variant(variant_decl) => {
                    tir_module
                        .variants
                        .push(self.reify_variant_decl(variant_decl));
                }
                Item::Test(test_decl) => {
                    let test_index = tir_module.tests.len();
                    let module_is_todo = module.has_todo();
                    if let Some((tir_func, tir_test)) =
                        self.reify_test_decl(test_decl, test_index, module_is_todo)
                    {
                        tir_module.add_function(tir_func);
                        tir_module.tests.push(tir_test);
                    }
                }
                Item::Global(global_decl) => {
                    // Globals are not gated: a global initializer can trap or
                    // perform effects at module-init time (`global _X = panic(…)`
                    // must still trap even if `_X` is never read), and purity
                    // analysis cannot see divergence through ambient `panic`.
                    // The optimize-time DCE removes genuinely pure dead globals
                    // instead. A dead global is still reported as a warning.
                    if let Some(tir_global) = self.reify_global(global_decl) {
                        tir_module.globals.push(tir_global);
                    }
                }
                Item::Enum(enum_decl) => {
                    tir_module.add_enum(self.reify_enum(enum_decl));
                }
                Item::Flags(flags_decl) => {
                    if let Some(tir_flags) = self.reify_flags(flags_decl) {
                        tir_module.add_flags(tir_flags);
                    }
                }
                Item::Newtype(newtype_decl) => {
                    if let Some(tir_newtype) = self.reify_newtype(newtype_decl) {
                        tir_module.add_newtype(tir_newtype);
                    }
                }
                Item::Interface(effect_decl) => {
                    tir_module.add_effect(self.reify_effect_decl(effect_decl));
                    // An operation's default body is an ordinary function under
                    // a synthesized name; the dispatch wrapper's no-handler
                    // branch calls it. Never dead-item-filtered: the only call
                    // to it is synthesized after liveness ran.
                    for method in default_impl_methods(effect_decl) {
                        if let Some(tir_func) = self.reify_function(&method) {
                            tir_module.add_function(tir_func);
                        }
                    }
                }
                Item::Resource(resource_decl) => {
                    tir_module.add_resource(self.reify_resource_decl(resource_decl));
                }
                _ => {}
            }
        }

        // Share the type table via Rc::clone so downstream phases see
        // the same arena reify just interned into.
        tir_module.type_table = Rc::clone(&self.tysys.type_table);

        if let Some(data) = module.data_section() {
            tir_module = tir_module.with_data_section(Some(data.to_string()));
        }

        // Anonymous structs synthesised during body resolution by
        // `annotate_bodies` live on `sem.decls.pending_anonymous_structs`.
        // Reify clones them into the emitted module rather than draining,
        // because `sem` is `&` here.
        for anon_struct in &self.sem.decls.pending_anonymous_structs {
            tir_module.add_struct(anon_struct.clone());
        }
        // Local item declarations (`Stmt::Item`) discovered and built by
        // `reify_local_item` while reifying the item loop above (function/
        // method/test bodies). Reify-owned, so drained rather than cloned.
        for local_struct in std::mem::take(&mut self.pending_local_structs) {
            tir_module.add_struct(local_struct);
        }
        for local_newtype in std::mem::take(&mut self.pending_local_newtypes) {
            tir_module.add_newtype(local_newtype);
        }

        // Forward the per-module synthesis requests annotate recorded on
        // `ModuleDecls`. Default-method synthesis is reify's own job, handled
        // per impl by `reify_impl_default_methods` in the `Item::Impl` arm.
        for req in &self.sem.decls.pending_synthesis_requests {
            tir_module.synthesis_requests.push(req.clone());
        }

        tir_module.wasm_module = module.wasm_module().map(String::from);

        self.logger.ok_or_bail(tir_module)
    }

    // ─────────────────────────────────────────────────────────────────
    // Decl-only items: read from `tysys.all_*` and produce TIR without
    // consulting `TypeAnnotations`.
    // ─────────────────────────────────────────────────────────────────

    /// Reify an `enum E { … }` declaration. Pure projection from the
    /// AST shape; cases keep their declared index.
    fn reify_enum(&self, enum_decl: &ast::EnumDecl) -> TirEnum {
        TirEnum {
            def: self
                .tysys
                .resolutions
                .defs()
                .of_ast_id(enum_decl.id)
                .expect("an `enum` declaration is declared"),
            name: enum_decl.name.clone(),
            module_source: self.current_module_source.clone(),
            visibility: enum_decl.visibility,
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
                    wire_name_override: wire_name_override_of(&case.attrs),
                })
                .collect(),
            span: enum_decl.span,
            wire_name_policy: wire_name_policy_of(&enum_decl.attrs),
        }
    }

    /// Reify a `flags F { … }` declaration. The `TypeId` is the one
    /// `annotate_decls` interned via `make_flags`; reify reads it from
    /// `tysys.all_flags_cases`.
    fn reify_flags(&self, flags_decl: &ast::FlagsDecl) -> Option<TirFlags> {
        let def = self.tysys.resolutions.defs().of_ast_id(flags_decl.id)?;
        let info = self.tysys.all_flags_cases.get(&def)?;
        Some(TirFlags {
            def,
            name: flags_decl.name.clone(),
            module_source: self.current_module_source.clone(),
            visibility: flags_decl.visibility,
            type_id: info.type_id,
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
            wire_name_policy: wire_name_policy_of(
                flags_decl.attributes.as_deref().unwrap_or_default(),
            ),
        })
    }

    /// A newtype's parameters as the impls written over the declaration see
    /// them, read off what the declaration states. A default is the one fact
    /// left behind: resolving it needs the declaration's own parameters in
    /// scope, which is where the base type is resolved, not here.
    fn declared_type_params(params: &[ast::GenericParam]) -> Vec<crate::tir::TirTypeParam> {
        params
            .iter()
            .enumerate()
            .map(|(index, p)| crate::tir::TirTypeParam {
                name: p.name.clone(),
                is_effect: p.is_effect,
                is_pack: p.is_pack,
                bounds: p.real_bounds().iter().map(|b| b.name.clone()).collect(),
                default: None,
                index: index as u32,
                projected_from: None,
            })
            .collect()
    }

    /// Reify a `type N = T;` declaration. A generic one names no single type —
    /// each instantiation is its own, minted on demand by
    /// `make_newtype_instance` — so it lands here without one, carrying the
    /// parameters its synthesized impls are written over.
    fn reify_newtype(&self, newtype_decl: &ast::Newtype) -> Option<TirNewtype> {
        let def = self.tysys.resolutions.defs().of_ast_id(newtype_decl.id)?;
        let generic = !newtype_decl.type_params.is_empty();
        let type_id = self.tysys.all_newtypes.get(&def).copied();
        if !generic && type_id.is_none() {
            return None;
        }
        Some(TirNewtype {
            name: newtype_decl.name.clone(),
            module_source: self.current_module_source.clone(),
            visibility: newtype_decl.visibility,
            def,
            type_params: Self::declared_type_params(&newtype_decl.type_params),
            type_id: type_id.filter(|_| !generic),
            wire_name_policy: wire_name_policy_of(&newtype_decl.attrs),
            span: newtype_decl.span,
        })
    }

    // ─────────────────────────────────────────────────────────────────
    // Items with sub-declaration walks but no function bodies.
    // ─────────────────────────────────────────────────────────────────

    /// Reify a `struct S { … }`. Field types come from
    /// `tysys.all_struct_fields`; field-default expressions and
    /// type-param defaults are read from `ModuleSemantics`.
    fn reify_struct(&mut self, struct_decl: &ast::StructDecl) -> TirStruct {
        // Single source of truth: `resolve_struct` recorded the per-field
        // resolved types (with the decl's type-param scope alive and
        // `loaded_modules` available, so `pub use` re-export chains are
        // followed). Reify reads them straight from the annotation rather
        // than reading the static decl-field pass output and masking
        // UNKNOWNs with its own re-resolve.
        let field_types = self
            .ann_struct_field_types(struct_decl.id)
            .expect("resolve_struct records the field types for every struct reify emits");

        // Field-default expressions resolve in a per-struct
        // `FunctionContext` keyed `struct:<name>` (no self, no other
        // fields in scope), matching `Elaborator::resolve_struct`
        // byte-for-byte so the synthesized purity check and reify see
        // identical TIR.
        let mut field_ctx = FunctionContext::new(
            crate::tir::TypeTable::UNIT,
            format!("struct:{}", struct_decl.name),
        );

        let mut fields = Vec::with_capacity(struct_decl.fields.len());
        for (index, field) in struct_decl.fields.iter().enumerate() {
            let type_id = field_types[index];

            let wire_name_override = wire_name_override_of(&field.attrs);

            let default_expr: Option<Box<TirExpr>> = field.default.as_ref().map(|default_ast| {
                Box::new(self.reify_expr(default_ast, &mut field_ctx, Some(type_id)))
            });

            // A field is optional on deserialize iff it has a default value.
            // `#[wire(default)]` is removed (rejected in `resolve_struct`).
            let serde_default = field.default.is_some();

            let serde_positional = field
                .attrs
                .iter()
                .any(|a| a.name == "wire" && a.has_arg("positional"));

            fields.push(crate::tir::TirField {
                name: field.name.clone(),
                visibility: field.visibility,
                type_id,
                index: index as u32,
                span: field.span,
                is_secret: field.attrs.iter().any(|a| a.name == "secret"),
                wire_name_override,
                serde_default,
                serde_positional,
                default_expr,
            });
        }

        // Single source of truth: the body walk projected these type
        // params with each default resolved while the decl's type-param scope
        // was alive; read them back rather than re-resolving the defaults.
        let type_params = self
            .ann_decl_type_params(struct_decl.id)
            .expect("resolve_struct records the type params for every struct reify emits");

        let wire_name_policy = wire_name_policy_of(&struct_decl.attrs);

        TirStruct {
            def: crate::tir::StructDef::Decl(
                self.tysys
                    .resolutions
                    .defs()
                    .of_ast_id(struct_decl.id)
                    .expect("a `struct` declaration is declared"),
            ),
            type_args: Vec::new(),
            name: struct_decl.name.clone(),
            module_source: self.current_module_source.clone(),
            visibility: struct_decl.visibility,
            type_params,
            monomorph_info: None,
            fields,
            span: struct_decl.span,
            wire_name_policy,
        }
    }

    /// Reify a local item declaration (`Stmt::Item` — a `struct`/`type`
    /// declared inside a function body). Unlike `reify_struct`/
    /// `reify_newtype`, there is no `Item::Struct`/`Item::Newtype` entry in
    /// `module.items` for the per-item dispatch loop (`reify_module`) to
    /// walk — this `reify_stmt` arm is the only place a local item's TIR is
    /// discovered — so the result accumulates on `self.pending_local_structs`
    /// / `self.pending_local_newtypes`, flushed into the module's TIR
    /// alongside `pending_anonymous_structs` at the end of `reify_module`.
    fn reify_local_item(&mut self, item: &ast::Item) {
        match item {
            ast::Item::Struct(struct_decl) => self.reify_local_struct(struct_decl),
            ast::Item::Newtype(newtype_decl) => self.reify_local_newtype(newtype_decl),
            _ => {}
        }
    }

    /// Field types and type-param bounds come from
    /// `sem.decls.local_struct_fields` — the durable fact
    /// `resolve_local_struct` recorded under this declaration's own identity.
    /// Field attributes (`#[wire(...)]`, `#[secret]`) and default-value
    /// expressions are read straight from the AST here, exactly matching
    /// `reify_struct`'s handling for a top-level struct — `StructFieldInfo`
    /// doesn't carry attributes, only `(name, type, visibility)`.
    fn reify_local_struct(&mut self, struct_decl: &ast::StructDecl) {
        let Some(info) = self
            .tysys
            .resolutions
            .defs()
            .of_ast_id(struct_decl.id)
            .and_then(|def| self.sem.decls.local_struct_fields.get(&def))
            .cloned()
        else {
            // `resolve_local_struct` inserts this unconditionally for every
            // local struct declaration annotate resolved.
            return;
        };
        // Field-default expressions resolve in a per-struct `FunctionContext`
        // (no self, no other fields in scope), matching `reify_struct`.
        let mut field_ctx = FunctionContext::new(
            crate::tir::TypeTable::UNIT,
            format!("struct:{}", struct_decl.name),
        );
        let fields: Vec<crate::tir::TirField> = info
            .fields
            .iter()
            .enumerate()
            .map(|(index, (name, type_id, visibility))| {
                let field = struct_decl.fields.get(index);
                let attrs: &[ast::Attribute] = field.map_or(&[], |f| &f.attrs);
                let default_expr: Option<Box<TirExpr>> =
                    field.and_then(|f| f.default.as_ref()).map(|default_ast| {
                        Box::new(self.reify_expr(default_ast, &mut field_ctx, Some(*type_id)))
                    });
                crate::tir::TirField {
                    name: name.clone(),
                    visibility: *visibility,
                    type_id: *type_id,
                    index: index as u32,
                    span: field.map_or(struct_decl.span, |f| f.span),
                    is_secret: attrs.iter().any(|a| a.name == "secret"),
                    wire_name_override: wire_name_override_of(attrs),
                    serde_default: field.is_some_and(|f| f.default.is_some()),
                    serde_positional: attrs
                        .iter()
                        .any(|a| a.name == "wire" && a.has_arg("positional")),
                    default_expr,
                }
            })
            .collect();
        // Single source of truth, as for a top-level struct: the body walk
        // projected these with the struct's own type-param scope alive.
        let type_params = self.ann_decl_type_params(struct_decl.id).expect(
            "resolve_local_struct records the type params for every local struct reify emits",
        );
        self.pending_local_structs.push(TirStruct {
            def: crate::tir::StructDef::Decl(
                self.tysys
                    .resolutions
                    .defs()
                    .of_ast_id(struct_decl.id)
                    .expect("a function-local `struct` is declared"),
            ),
            type_args: Vec::new(),
            name: info.name,
            module_source: self.current_module_source.clone(),
            visibility: ast::Visibility::Private,
            type_params,
            monomorph_info: None,
            fields,
            span: struct_decl.span,
            wire_name_policy: None,
        });
    }

    /// The base type comes from `sem.decls.local_newtypes` — the durable
    /// fact `resolve_local_newtype` recorded under this declaration's own
    /// identity.
    fn reify_local_newtype(&mut self, newtype_decl: &ast::Newtype) {
        let Some(def) = self.tysys.resolutions.defs().of_ast_id(newtype_decl.id) else {
            return;
        };
        let generic = !newtype_decl.type_params.is_empty();
        let type_id = self.sem.decls.local_newtypes.get(&def).copied();
        if !generic && type_id.is_none() {
            return;
        }
        self.pending_local_newtypes.push(TirNewtype {
            name: crate::name::mangle_local_item_name(&newtype_decl.name, newtype_decl.id),
            module_source: self.current_module_source.clone(),
            visibility: ast::Visibility::Private,
            def,
            type_params: Self::declared_type_params(&newtype_decl.type_params),
            type_id: type_id.filter(|_| !generic),
            wire_name_policy: wire_name_policy_of(&newtype_decl.attrs),
            span: newtype_decl.span,
        });
    }

    /// Reify a `variant V<T> { … }` declaration. Cases' payload types
    /// come from `tysys.all_variant_cases`; the type-param table is
    /// projected from the AST.
    fn reify_variant_decl(&mut self, variant_decl: &ast::VariantDecl) -> TirVariantDecl {
        let def = self
            .tysys
            .resolutions
            .defs()
            .of_ast_id(variant_decl.id)
            .expect("a `variant` declaration is declared");
        let case_info = self.tysys.all_variant_cases.get(&def);

        let cases: Vec<tir::TirVariantCase> = variant_decl
            .cases
            .iter()
            .enumerate()
            .map(|(index, case)| {
                let payload = case_info
                    .and_then(|info| info.cases.get(index).map(|c| c.payload))
                    .unwrap_or(crate::tir::TypeTable::UNIT);
                tir::TirVariantCase {
                    name: case.name.clone(),
                    index: index as u32,
                    payload,
                    span: case.span,
                    wire_name_override: wire_name_override_of(&case.attrs),
                }
            })
            .collect();

        // Single source of truth: read the type params the body walk
        // projected (defaults resolved with the decl's scope alive) rather
        // than re-resolving the defaults here.
        let type_params = self
            .ann_decl_type_params(variant_decl.id)
            .expect("resolve_variant_decl records the type params for every variant reify emits");
        self.tysys.type_table.borrow_mut().register_variant_cases(
            def,
            cases
                .iter()
                .map(|c| (c.name.clone(), c.index, c.payload))
                .collect(),
        );

        TirVariantDecl {
            def,
            name: variant_decl.name.clone(),
            module_source: self.current_module_source.clone(),
            visibility: variant_decl.visibility,
            type_params,
            cases,
            span: variant_decl.span,
            wire_name_policy: wire_name_policy_of(&variant_decl.attrs),
        }
    }

    /// Reify an `interface E { … }` declaration. Effects have no
    /// `Self` type — `&self` / `&mut self` on an effect method is a
    /// surface error annotate already diagnosed.
    fn reify_effect_decl(&mut self, decl: &ast::InterfaceDecl) -> tir::TirEffect {
        // Single source of truth: the body walk resolved the op
        // signatures with the decl's type-param / `Self` scope in place and
        // recorded them; reify reads them back rather than re-resolving.
        let operations = self
            .ann_effect_ops(decl.id)
            .expect("resolve_effect_decl records op signatures for every effect reify emits");
        tir::TirEffect {
            name: decl.name.clone(),
            visibility: decl.visibility,
            operations,
            span: decl.span,
        }
    }

    /// Reify a `resource R<T> { … }` declaration. Resource methods take
    /// a synthesised `self` parameter (`&Self` or `&mut Self`) at index
    /// 0; for generic resources `Self = GenericResource<…>` with the
    /// decl's own `TypeParam`s as type args. The op signatures are read
    /// from the facts the body walk recorded.
    fn reify_resource_decl(&mut self, decl: &ast::ResourceDecl) -> tir::TirResource {
        let operations = self
            .ann_effect_ops(decl.id)
            .expect("resolve_resource_decl records op signatures for every resource reify emits");
        tir::TirResource {
            def: self
                .tysys
                .resolutions
                .defs()
                .of_ast_id(decl.id)
                .expect("a `resource` declaration is declared"),
            name: decl.name.clone(),
            visibility: decl.visibility,
            operations,
            is_generic: !decl.type_params.is_empty(),
            span: decl.span,
        }
    }

    // ─────────────────────────────────────────────────────────────────
    // Items with function bodies — the bulk of reify_*.
    // ─────────────────────────────────────────────────────────────────

    /// Reify a free function. Builds a fresh `FunctionContext`, walks
    /// params + body, and assembles `TirFunction`. All inference
    /// decisions are read from `sem.types`; reify only emits TIR.
    ///
    /// Parameters are added in declaration order to pin the walk-order
    /// invariant.
    fn reify_function(&mut self, func: &ast::Function) -> Option<TirFunction> {
        // Single source of truth: read the (post-async-erasure) return type
        // `resolve_function` resolved, rather than re-reading the fragile
        // name-keyed `function_return_types` map (shared with call sites and
        // overwritable by later registrations).
        let return_type = self
            .ann_fn_return_type(func.id)
            .expect("resolve_function records the return type for every function reify emits");

        let mut ctx = FunctionContext::new(return_type, func.name.clone());
        if func.is_async {
            ctx.is_async = true;
            ctx.task_return_type = Some(return_type);
        }

        // Real type params only (effect params and `<F: fn(...)>` bounds
        // are excluded), so the positional indices stay dense and match
        // the emitted `type_params` and monomorph's substitution keys.
        let type_param_names: Vec<String> = func
            .type_params
            .iter()
            .filter(|p| p.is_real_type_param())
            .map(|p| p.name.clone())
            .collect();

        // Effect params (`<effect E>`) drive `Param` effect resolution in
        // function-type params; publish them for the body walk.
        let effect_param_names: Vec<String> = func
            .type_params
            .iter()
            .filter(|p| p.is_effect)
            .map(|p| p.name.clone())
            .collect();
        let saved_effect_param_names =
            std::mem::replace(&mut self.current_effect_param_names, effect_param_names);

        // Publish the body's type-param scope (see `reify_method`).
        let saved_type_param_names =
            std::mem::replace(&mut self.current_type_param_names, type_param_names);

        // Single source of truth: read the resolved param types
        // `resolve_function` recorded (in `func.params` order, with `<F: fn>`
        // bounds already realised), rather than re-resolving each here.
        let param_types = self
            .ann_fn_param_types(func.id)
            .expect("resolve_function records param types for every function reify emits");
        let mut params = Vec::with_capacity(func.params.len());
        for (p_idx, param) in func.params.iter().enumerate() {
            let type_id = *param_types
                .get(p_idx)
                .expect("resolve_function records one param type per func.params entry");
            // Do not reify the parameter's default here: defaults are expanded
            // (and reified) at each call site by `reify_pad_args_with_defaults`.
            // Reifying one into this function's own `ctx` would allocate a
            // control-flow default's value local in the callee, surfacing as a
            // parameter-shadowing `let` in the body at -O0 (returning the
            // zero-initialised shadow).
            let index = ctx.add_local_at(
                param.name.clone(),
                type_id,
                param.is_mut,
                Some(param.id),
                param.name_span,
            );
            params.push(tir::TirParam {
                name: param.name.clone(),
                type_id,
                local_index: index,
                is_mut: param.is_mut,
                is_mut_ref: false,
                span: param.span,
            });
        }

        let body = func
            .body
            .as_ref()
            .map(|b| self.reify_block(b, &mut ctx, None));

        self.current_type_param_names = saved_type_param_names;
        self.current_effect_param_names = saved_effect_param_names;

        // Single source of truth: read the TIR type params `resolve_function`
        // projected (effect / `fn`-bound params filtered, dense indices,
        // defaults resolved with the type-param scope alive), rather than
        // re-projecting them here after the scope is torn down.
        let type_params = self
            .ann_decl_type_params(func.id)
            .expect("resolve_function records the type params for every function reify emits");

        Some(TirFunction {
            module_source: ModuleSource::default(),
            name: func.name.clone(),
            def_id: self.def_of(func.id),
            visibility: func.visibility,
            is_export: func.is_export,
            is_async: func.is_async,
            type_params,
            impl_type_params: vec![],
            monomorph_info: None,
            method_info: None,
            params,
            return_type,
            // Async functions erase `return_type` to `()`; the real
            // (declared) return travels via `task return` and is recorded
            // by annotate in `function_task_returns`. Resource-store
            // inference (effect_check) walks `task_return_type`, so it
            // must carry the real type, not the erased unit. (Methods are
            // not recorded there and fall back to `return_type`.)
            task_return_type: if func.is_async {
                Some(
                    self.sem
                        .types
                        .function_task_returns
                        .get(&func.id)
                        .copied()
                        .unwrap_or(return_type),
                )
            } else {
                None
            },
            effects: self
                .sem
                .types
                .function_effects
                .get(&func.id)
                .cloned()
                .expect("resolve_function/resolve_method records function_effects for every function reify emits"),
            stores: func.stores.clone(),
            body,
            span: func.span,
            local_count: ctx.next_local,
            locals: ctx.locals.clone(),
            address_taken_locals: ctx.address_taken_locals,
            stores_aliased_locals: crate::hashmap::IndexSet::default(),
            is_cm_binding: false,
            is_dispatch_wrapper: false,
            is_cm_export: false,
            is_ambient: extract_is_ambient_attr(&func.attrs),
            benign_effects: self.reify_effects(&extract_benign_effect_names(&func.attrs)),
            inline_hint: extract_inline_hint_attr(&func.attrs),
            compiler_item: crate::elaborator::item::extract_compiler_item(
                &func.attrs,
                func.span,
                &self.current_module_source,
                self.logger,
            ),
            export_name: extract_export_name_attr(&func.attrs),
            allocator_tag: extract_allocator_tag_attr(&func.attrs),
            kind: tir::FunctionKind::Regular,
            return_abi: tir::ReturnAbi::Single,
        })
    }

    /// Reify the impl block's own declaration — its identity and rest
    /// clause, with no methods (those go through [`Self::reify_impl`] into
    /// `TirModule::functions`). A block whose only content is a rest clause
    /// still produces a record, which is the point: the effect-dispatch
    /// synthesis has no method to learn about it from.
    fn reify_impl_decl(&mut self, impl_block: &ast::ImplBlock) -> Option<crate::tir::TirImpl> {
        use crate::name::LocalMethodName;

        if impl_block.is_synthesize_request {
            return None;
        }
        let facts = self.sem.types.impl_facts.get(&impl_block.id)?;

        // Derive the target's name the way `reify_method` derives it for this
        // block's methods, so the block and its methods agree on the key the
        // effect-dispatch handler index is built from.
        let mut naming = LocalMethodName::of(
            facts.receiver.clone(),
            facts.trait_name.clone(),
            String::new(),
        );
        if let Some(owner) = facts.concrete_owner.as_ref() {
            naming = naming.with_substituted_struct_name(owner);
        }

        Some(crate::tir::TirImpl {
            trait_canonical: facts
                .trait_name
                .as_ref()
                .and_then(|fq| Some((fq.module()?.clone(), fq.base_name().to_string()))),
            trait_type_args: facts.trait_type_args.clone(),
            struct_name: naming.struct_name(),
            rest: impl_block.rest.map(|r| r.kind),
            span: impl_block.span,
        })
    }

    /// Reify every method on an `impl` block, threading
    /// `sem.types.impl_facts[impl_block.id]` into [`Self::reify_method`].
    ///
    /// Synthesis requests and default-method synthesis are out of scope here:
    /// both live on `sem.decls` and are aggregated by [`Self::reify_module`].
    fn reify_impl(&mut self, impl_block: &ast::ImplBlock) -> Vec<TirFunction> {
        if impl_block.is_synthesize_request {
            return Vec::new();
        }
        let impl_key = impl_block.id;
        let Some(facts) = self.sem.types.impl_facts.get(&impl_key).cloned() else {
            // Annotate did not record facts — the impl block was
            // diagnosed by annotate (e.g. unknown trait reference)
            // and skipped. Reify follows by emitting no methods.
            return Vec::new();
        };

        // Per-instantiation owner (`"List<u8>"`) for a fully concrete impl —
        // decided AST-side by the elaborator and recorded on the facts so it
        // agrees with method dispatch's `from_concrete_impl` (and is not fooled
        // by a param named like a known type). Methods become concrete fns.
        let concrete_owner: Option<FqTypeName> = facts.concrete_owner.clone();

        impl_block
            .methods
            .iter()
            .filter_map(|method| {
                if self.is_dead_item(method.id) {
                    return None;
                }
                self.reify_method(method, &facts, concrete_owner.as_ref())
            })
            .collect()
    }

    /// Synthesise a `Struct^Trait::method` `TirFunction` for each default method
    /// the impl does not override, reading the body-walk facts from
    /// `sem.default_method_semantics[(impl_block.id, default_method.id)]`. The
    /// module perspective is swapped to the trait module for the walk, since the
    /// facts are keyed by `AstId`s that name it.
    fn reify_impl_default_methods(&mut self, impl_block: &ast::ImplBlock) -> Vec<TirFunction> {
        use crate::name::MethodName;

        if impl_block.is_synthesize_request {
            return Vec::new();
        }
        let Some(trait_ast) = impl_block.trait_type.as_ref() else {
            return Vec::new();
        };
        let impl_key = impl_block.id;
        let Some(facts) = self.sem.types.impl_facts.get(&impl_key).cloned() else {
            return Vec::new();
        };
        let Some(trait_name_mangled) = facts.trait_name.clone() else {
            return Vec::new();
        };
        let struct_name = facts.struct_name.clone();

        // Concrete generic instantiation owner (`"List<u8>"`) for
        // `impl Tag for List<u8>`, so default methods are also per-instantiation
        // concrete functions. Recorded AST-side by the elaborator.
        let concrete_owner: Option<FqTypeName> = facts.concrete_owner.clone();

        // The impl header names the trait at a site of its own, which the walk
        // answered for in the module that wrote the header.
        let Some(trait_decl) = crate::resolve::head_site(trait_ast)
            .and_then(|site| self.tysys.resolutions.declared(site))
        else {
            return Vec::new();
        };
        let Some(trait_sig) = super::trait_query::trait_sig_of_with(
            trait_decl,
            &self.tysys.trait_env,
            &self.tysys.signatures,
        ) else {
            return Vec::new();
        };
        let provided: crate::hashmap::IndexSet<&str> =
            impl_block.methods.iter().map(|m| m.name.as_str()).collect();
        let default_methods: Vec<std::rc::Rc<ast::Function>> = trait_sig
            .default_methods()
            .filter(|(name, _)| !provided.contains(name))
            .map(|(_, body)| std::rc::Rc::clone(body))
            .collect();
        let trait_module = trait_sig.module.clone();

        let trait_items: &'a [ast::Item] = self
            .loaded_modules
            .get(&trait_module)
            .map(|m| m.items.as_slice())
            .unwrap_or(&[]);

        let mut out = Vec::with_capacity(default_methods.len());
        for default_method in &default_methods {
            // The per-impl `ModuleSemantics` snapshot lives on the parent
            // `self.sem.default_method_semantics`. Borrow it at `'a` — the
            // parent is `&'a ModuleSemantics`, so `IndexMap::get` returns
            // `Option<&'a ModuleSemantics>`, which is exactly what
            // `self.sem` accepts in the swap.
            let key = (impl_block.id, default_method.id);
            let Some(synth_sem) = self.sem.default_method_semantics.get(&key) else {
                // Combined walk did not record a synthesis for this default
                // method (e.g. `resolve_method` returned `None` in
                // error-recovery). Skip — reify produces no TIR for it.
                continue;
            };

            // Swap perspective. Both `self.sem` and the swap target carry
            // lifetime `'a` (from the parent `&'a ModuleSemantics`), so
            // the `mem::replace` is a pointer swap.
            let saved_sem = std::mem::replace(&mut self.sem, synth_sem);
            let saved_module_source =
                std::mem::replace(&mut self.current_module_source, trait_module.clone());
            let saved_module_items = std::mem::replace(&mut self.current_module_items, trait_items);

            let tir_func_opt = self.reify_method(default_method, &facts, concrete_owner.as_ref());

            self.current_module_items = saved_module_items;
            self.current_module_source = saved_module_source;
            self.sem = saved_sem;

            if let Some(mut tir_func) = tir_func_opt {
                // `resolve_method` records this exact string under
                // `method_names`, which `reify_method` already read back.
                // Recomputing it covers the synthesis path, where the fact
                // has no declaring walk to come from, and goes through the
                // same `format_local` so the two spellings agree.
                tir_func.name = MethodName::format_local(
                    concrete_owner.as_ref().unwrap_or(&struct_name),
                    Some(&trait_name_mangled),
                    &default_method.name,
                );
                // Default methods from trait declarations are not marked
                // pub in the AST, but they should be treated as pub since
                // they are part of a trait implementation.
                tir_func.visibility = crate::ast::Visibility::Public;
                out.push(tir_func);
            }
        }

        out
    }

    /// Reify a single method inside an `impl` block. The method's
    /// body walk shares the structure with [`Self::reify_function`];
    /// the difference is that the receiver (`&self` / `&mut self`)
    /// is synthesised from the recorded [`super::sem::types::ImplFacts::self_type`]
    /// (no re-resolution of the impl target), and the resulting
    /// [`TirFunction`] carries the `method_info` /
    /// `impl_type_params` reify reads from the same recorded facts.
    fn reify_method(
        &mut self,
        func: &ast::Function,
        facts: &super::sem::types::ImplFacts,
        // `Some("List<u8>")` when the impl is on a concrete generic
        // instantiation. The method is then a per-instantiation *concrete*
        // function: named `List<u8>::method`, with no impl type params and no
        // monomorphization, so distinct instantiations stay distinct and call
        // sites resolve it directly (mirroring a monomorphized instance).
        concrete_owner: Option<&FqTypeName>,
    ) -> Option<TirFunction> {
        use crate::ast::SelfKind;
        use crate::name::LocalMethodName;

        // Single source of truth: the impl-type-param scheme is computed once
        // by `Elaborator::resolve_method` and recorded; reify reads it. reify
        // runs only for the current module's explicitly-written methods in the
        // same `build_tir_from_state` pass that recorded them (stdlib is
        // rehydrated from the snapshot's already-reified TIR), so the fact is
        // always present — a missing entry is a contract violation, not a
        // fallback case.
        let mut impl_type_params: Vec<crate::tir::TirTypeParam> =
            self.ann_method_impl_type_params(func.id).expect(
                "resolve_method records the impl-type-param scheme for every \
                 impl method reify emits",
            );

        // Type-param scope for the method's own param/return types. Every
        // impl-self-type arg occupies its positional slot, concrete ones
        // (`String` in `TreeMap<String, V>`) included — monomorph substitutes
        // those back by identity. Method-level params continue after the impl
        // param count, the same base `func_inst::instantiate_function` uses.
        let mut type_param_names: Vec<String> = Vec::new();
        for p in &impl_type_params {
            let idx = p.index as usize;
            if type_param_names.len() <= idx {
                type_param_names.resize(idx + 1, String::new());
            }
            type_param_names[idx].clone_from(&p.name);
        }
        let mut next_idx = impl_type_params.len();
        for p in &func.type_params {
            // Skip `<F: fn(...)>` bounds: the elaborator realises them
            // eagerly to the bound's function type (already baked into the
            // recorded param/return types), so they must not consume a
            // positional type-param slot or the real method params shift index.
            if !p.is_real_type_param() || type_param_names.iter().any(|n| n == &p.name) {
                continue;
            }
            if type_param_names.len() <= next_idx {
                type_param_names.resize(next_idx + 1, String::new());
            }
            type_param_names[next_idx].clone_from(&p.name);
            next_idx += 1;
        }

        // Method-level effect params (`<effect E>`) drive `Param` effect
        // resolution in function-type params; publish them for the method
        // body walk.
        let effect_param_names: Vec<String> = func
            .type_params
            .iter()
            .filter(|p| p.is_effect)
            .map(|p| p.name.clone())
            .collect();
        let saved_effect_param_names =
            std::mem::replace(&mut self.current_effect_param_names, effect_param_names);

        // Single source of truth: the impl block's mangled struct name as
        // the elaborator computed it via `get_type_name(&impl_block.ty)`
        // (recorded on `ImplFacts::struct_name`). Reconstructing it from
        // `facts.self_type` would need the `&` / `&mut` / tuple
        // special-cases and the `&T`-blanket "bare `&`" carve-out that
        // `get_type_name` already encodes — exactly the
        // parity-bug class WEP 2026-05-26 §"Reify — mechanical" calls out.
        let _base_struct_name = facts.struct_name.clone();
        // Mangled / display names — read straight off the per-method facts
        // `resolve_method` already publishes; reify no longer runs
        // `format_local` itself.
        let method_names = self.ann_method_names(func.id).expect(
            "resolve_method records the mangled + display names for every impl method reify emits",
        );
        let display_name = method_names.display;
        let mut mangled_name = method_names.mangled;
        let mut method_info = {
            let mut info = LocalMethodName::of(
                facts.receiver.clone(),
                facts.trait_name.clone(),
                func.name.clone(),
            );
            info.is_ref_impl = facts.is_ref_impl;
            // Carry the impl's trait type args (`impl Future<i32> for …`
            // → `[i32]`). The effect-dispatch synthesis keys its handler
            // index on `(struct, effect_module, base_trait, trait_type_args)`;
            // without the args a generic-effect
            // handler is keyed `Future<>` and the `Future<i32>` binding
            // finds no `DispatchPlan`.
            info.trait_type_args.clone_from(&facts.trait_type_args);
            info
        };

        // Concrete generic instantiation (`impl List<u8>`): emit a
        // per-instantiation concrete function. Its name and `method_info`
        // carry the concrete owner (`List<u8>`), it has no impl type params,
        // and it is not monomorphized — structurally identical to a
        // monomorphized instance, so DCE / WIR / cross-module inclusion all
        // handle it, and `impl List<u8>` vs `impl List<i32>` stay distinct.
        if let Some(owner) = concrete_owner {
            mangled_name =
                crate::name::MethodName::format_local(owner, facts.trait_name.as_ref(), &func.name);
            method_info = method_info.with_substituted_struct_name(owner);
            impl_type_params = Vec::new();
        }

        // Single source of truth: read the return type `resolve_method`
        // resolved, rather than re-resolving the return annotation.
        let return_type = self
            .ann_fn_return_type(func.id)
            .expect("resolve_method records the return type for every impl method reify emits");

        let mut ctx = FunctionContext::new(return_type, display_name);
        ctx.in_handler_method = facts.is_handler_method;
        if func.is_async {
            ctx.is_async = true;
            ctx.task_return_type = Some(return_type);
        }

        // Publish the body's type-param scope so turbofish args in the
        // body (`v.serialize::<S>(s)`) resolve against it. Restored before
        // returning so decl-level resolution stays scope-free.
        let saved_type_param_names =
            std::mem::replace(&mut self.current_type_param_names, type_param_names.clone());

        // Single source of truth: read the resolved param types
        // `resolve_method` recorded (in `func.params` order, receiver
        // included), rather than re-resolving each here.
        let param_types = self
            .ann_fn_param_types(func.id)
            .expect("resolve_method records param types for every impl method reify emits");
        let mut params = Vec::with_capacity(func.params.len());
        for (p_idx, p) in func.params.iter().enumerate() {
            let type_id = *param_types
                .get(p_idx)
                .expect("resolve_method records one param type per func.params entry");
            let name = if matches!(p.self_kind, SelfKind::None) {
                p.name.clone()
            } else {
                "self".to_string()
            };
            // See the free-function param loop: the param default is expanded
            // at call sites and monomorphize drops this field, so reifying it
            // into the method's `ctx` only pollutes its locals (a control-flow
            // default's value local shadows the parameter at -O0). Leave it
            // unbuilt.
            let local_index =
                ctx.add_local_at(name.clone(), type_id, p.is_mut, Some(p.id), p.name_span);
            params.push(crate::tir::TirParam {
                name,
                type_id,
                local_index,
                is_mut: p.is_mut,
                is_mut_ref: false,
                span: p.span,
            });
        }

        let body = func
            .body
            .as_ref()
            .map(|b| self.reify_block(b, &mut ctx, None));

        self.current_type_param_names = saved_type_param_names;
        self.current_effect_param_names = saved_effect_param_names;

        // Single source of truth: read the method-level type params
        // `resolve_method` projected (effect / `fn`-bound params filtered,
        // dense indices, defaults resolved with the type-param scope alive),
        // rather than re-projecting them here after the scope is torn down.
        let type_params = self.ann_decl_type_params(func.id).expect(
            "resolve_method records the method type params for every impl method reify emits",
        );

        Some(TirFunction {
            module_source: ModuleSource::default(),
            name: mangled_name,
            def_id: self.def_of(func.id),
            visibility: func.visibility,
            is_export: false,
            is_async: func.is_async,
            type_params,
            impl_type_params,
            monomorph_info: None,
            method_info: Some(method_info),
            params,
            return_type,
            // Async functions erase `return_type` to `()`; the real
            // (declared) return travels via `task return` and is recorded
            // by annotate in `function_task_returns`. Resource-store
            // inference (effect_check) walks `task_return_type`, so it
            // must carry the real type, not the erased unit. (Methods are
            // not recorded there and fall back to `return_type`.)
            task_return_type: if func.is_async {
                Some(
                    self.sem
                        .types
                        .function_task_returns
                        .get(&func.id)
                        .copied()
                        .unwrap_or(return_type),
                )
            } else {
                None
            },
            effects: self
                .sem
                .types
                .function_effects
                .get(&func.id)
                .cloned()
                .expect("resolve_function/resolve_method records function_effects for every function reify emits"),
            stores: func.stores.clone(),
            body,
            span: func.span,
            local_count: ctx.next_local,
            locals: ctx.locals.clone(),
            address_taken_locals: ctx.address_taken_locals,
            stores_aliased_locals: crate::hashmap::IndexSet::default(),
            is_cm_binding: false,
            is_dispatch_wrapper: false,
            is_cm_export: false,
            is_ambient: extract_is_ambient_attr(&func.attrs),
            benign_effects: self.reify_effects(&extract_benign_effect_names(&func.attrs)),
            inline_hint: extract_inline_hint_attr(&func.attrs),
            compiler_item: crate::elaborator::item::extract_compiler_item(
                &func.attrs,
                func.span,
                &self.current_module_source,
                self.logger,
            ),
            export_name: extract_export_name_attr(&func.attrs),
            allocator_tag: extract_allocator_tag_attr(&func.attrs),
            kind: crate::tir::FunctionKind::Regular,
            return_abi: crate::tir::ReturnAbi::Single,
        })
    }

    /// Reify a `test "…" { … }` block. Returns the synthesised
    /// `TirFunction` plus the `TirTest` metadata. Mirrors
    /// `Elaborator::resolve_test_decl`: the function
    /// name encodes `test_index` + attributes (`expect_trap`, `TODO`,
    /// `timeout_ms`); the body reifies into a unit-returning
    /// no-parameter function.
    fn reify_test_decl(
        &mut self,
        test_decl: &ast::TestDecl,
        test_index: usize,
        module_is_todo: bool,
    ) -> Option<(TirFunction, TirTest)> {
        use crate::tir::{FunctionKind, InlineHint, ReturnAbi, TypeTable};

        let meta = test_decl.metadata(module_is_todo);
        let ast::TestMetadata {
            expect_trap,
            is_todo,
            timeout_ms,
            is_synopsis,
        } = meta;
        let function_name =
            crate::name::test_function_name(&meta, test_index, test_decl.name.as_deref());

        let return_type = TypeTable::UNIT;
        let mut ctx = FunctionContext::new(return_type, function_name.clone());
        let body = self.reify_block(&test_decl.body, &mut ctx, None);

        let tir_func = TirFunction {
            module_source: ModuleSource::default(),
            name: function_name.clone(),
            def_id: None,
            visibility: crate::ast::Visibility::Private,
            is_export: false,
            is_async: false,
            type_params: vec![],
            impl_type_params: vec![],
            monomorph_info: None,
            method_info: None,
            params: vec![],
            return_type,
            task_return_type: None,
            effects: vec![],
            stores: vec![],
            body: Some(body),
            span: test_decl.span,
            local_count: ctx.next_local,
            locals: ctx.locals.clone(),
            address_taken_locals: ctx.address_taken_locals,
            stores_aliased_locals: crate::hashmap::IndexSet::default(),
            is_cm_binding: false,
            is_dispatch_wrapper: false,
            is_cm_export: false,
            is_ambient: false,
            benign_effects: Vec::new(),
            inline_hint: InlineHint::Auto,
            compiler_item: None,
            export_name: None,
            allocator_tag: None,
            kind: FunctionKind::Regular,
            return_abi: ReturnAbi::default(),
        };

        let tir_test = TirTest {
            name: test_decl.name.clone(),
            function_name,
            line: test_decl.span.line,
            span: test_decl.span,
            expect_trap,
            is_todo,
            timeout_ms,
            is_synopsis,
        };

        Some((tir_func, tir_test))
    }

    /// Reify a `global g: T = expr;` declaration. The declared type
    /// was already resolved by `annotate_decls` and lives on
    /// `sem.decls.current_module_globals`; reify reads it back and
    /// walks the initializer through a minimal `FunctionContext`.
    /// `is_nullable` / `lazy_init` are populated by the lower phase
    /// (kept `false` here, matching `Elaborator::resolve_global`).
    fn reify_global(&mut self, global_decl: &ast::GlobalDecl) -> Option<TirGlobal> {
        // `annotate_module_decls` populates `current_module_globals` for every
        // global it sees before any per-item reify runs, so the lookup
        // never misses; reify is a pure read.
        let ty = self
            .sem
            .decls
            .current_module_globals
            .get(&global_decl.name)
            .map(|(t, _)| *t)
            .expect("annotate_module_decls records every global in current_module_globals");

        let mut ctx = FunctionContext::new(
            ty,
            global_name(&self.current_module_source, &global_decl.name),
        );
        let initializer = self.reify_expr(&global_decl.initializer, &mut ctx, Some(ty));
        let param = self.reify_param_attr(global_decl);

        Some(TirGlobal {
            name: global_decl.name.clone(),
            ty,
            init: GlobalInit::Direct(initializer),
            param,
            wado_mutable: global_decl.mutable,
            visibility: global_decl.visibility,
            module_source: self.current_module_source.clone(),
            span: global_decl.span,
            locals: ctx.locals.clone(),
        })
    }

    /// Extract and structurally validate a `#[param]` attribute on a global.
    ///
    /// Returns `Some(ParamSpec)` for a well-formed `#[param]`, `None` when the
    /// global has no `#[param]` or the attribute is malformed (in which case a
    /// structural error is emitted; compilation bails at `ok_or_bail`). The
    /// resolution itself (overrides, env, conversion) happens later in the
    /// param-resolution pass — see `wep-2026-04-26-compile-time-params.md`.
    fn reify_param_attr(&self, global_decl: &ast::GlobalDecl) -> Option<tir::ParamSpec> {
        use crate::compiler_host::{Code, Diagnostic, DiagnosticSpan, Severity};

        let attr = global_decl.attributes.iter().find(|a| a.name == "param")?;
        let emit = |message: String| {
            let _ = self.logger.error_in(
                &self.current_module_source,
                Diagnostic {
                    severity: Severity::Error,
                    code: Code::ParamAttr,
                    message,
                    span: Some(DiagnosticSpan::from_span(&attr.span, None)),
                },
            );
        };

        let mut ok = true;
        if global_decl.mutable {
            emit("#[param] cannot be applied to a mutable global".to_string());
            ok = false;
        }

        for arg in &attr.args {
            match arg {
                ast::AttrArg::KeyValue(k, _) if k == "name" || k == "from_env" => {}
                ast::AttrArg::KeyValue(k, _) | ast::AttrArg::KeyArray(k, _) => {
                    emit(format!("unknown #[param] argument: {k}"));
                    ok = false;
                }
                ast::AttrArg::Str(s) | ast::AttrArg::Ident(s) | ast::AttrArg::Number(s) => {
                    emit(format!("unknown #[param] argument: {s}"));
                    ok = false;
                }
            }
        }

        let name = match attr.kv_value("name") {
            Some("") => {
                emit("#[param] name must not be empty".to_string());
                ok = false;
                global_decl.name.clone()
            }
            Some(n) => n.to_string(),
            None => global_decl.name.clone(),
        };
        let from_env = match attr.kv_value("from_env") {
            Some("") => {
                emit("#[param] from_env must not be empty".to_string());
                ok = false;
                None
            }
            Some(env) => Some(env.to_string()),
            None => None,
        };

        if ok {
            Some(tir::ParamSpec { name, from_env })
        } else {
            None
        }
    }

    // ─────────────────────────────────────────────────────────────────
    // Body walks: expressions, statements, blocks, patterns.
    // ─────────────────────────────────────────────────────────────────

    /// Reify a block expression — walks each statement in order so
    /// `FunctionContext::locals` matches what annotate produced.
    pub(super) fn reify_block(
        &mut self,
        block: &ast::Block,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
    ) -> TirBlock {
        self.reify_block_with_position(block, ctx, expected_type, false)
    }

    /// Reify a block whose value is consumed (expression position). Mirrors
    /// [`Elaborator::resolve_block_value`]: a trailing `match`/`if` keeps its
    /// value flowing out even without an `expected_type`, rather than being
    /// dropped at statement position.
    pub(super) fn reify_block_value(
        &mut self,
        block: &ast::Block,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
    ) -> TirBlock {
        self.reify_block_with_position(block, ctx, expected_type, true)
    }

    fn reify_block_with_position(
        &mut self,
        block: &ast::Block,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
        tail_value: bool,
    ) -> TirBlock {
        ctx.enter_scope();
        let stmts =
            self.reify_positioned_stmts(&block.stmts, block.span, ctx, expected_type, tail_value);
        ctx.exit_scope();
        TirBlock::new(stmts, block.span)
    }

    /// Reify a statement slice at block position. `block_span` is the enclosing
    /// block's span, used for synthetic continuation blocks. A `let ... else`
    /// consumes the *rest* of the slice as its then-arm (see
    /// [`Self::reify_let_else`]), so this is recursive.
    fn reify_positioned_stmts(
        &mut self,
        slice: &[ast::Stmt],
        block_span: crate::token::Span,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
        tail_value: bool,
    ) -> Vec<TirStmt> {
        let len = slice.len();
        let mut stmts = Vec::new();
        for (i, s) in slice.iter().enumerate() {
            // `reify_let_else` consumes the rest of the block as its then-arm,
            // so stop here after emitting it.
            if let ast::Stmt::Let(l) = s
                && l.else_block.is_some()
            {
                stmts.push(self.reify_let_else(
                    l,
                    &slice[i + 1..],
                    block_span,
                    ctx,
                    expected_type,
                    tail_value,
                ));
                break;
            }
            // Mirror `Elaborator::resolve_block_with_position` (stmt.rs): a
            // trailing `Expr` / `If` / `Match` / `LabeledBlock` keeps its
            // result flowing out as the block's value — when an
            // `expected_type` flows in (coercion) or the block sits in
            // expression position (`tail_value`) — rather than being dropped
            // at statement position.
            if (expected_type.is_some() || tail_value) && i == len - 1 {
                if let ast::Stmt::Expr(expr_stmt) = s {
                    let expr = self.reify_expr(&expr_stmt.expr, ctx, expected_type);
                    stmts.push(TirStmt::new(
                        crate::tir::TirStmtKind::Expr(expr),
                        expr_stmt.span,
                    ));
                    continue;
                }
                if let ast::Stmt::If(if_stmt) = s {
                    stmts.extend(self.reify_if_stmt_with_expected(
                        if_stmt,
                        ctx,
                        expected_type,
                        tail_value,
                    ));
                    continue;
                }
                if let ast::Stmt::Match(match_expr) = s {
                    let recorded = self
                        .ann_expression_types(match_expr.id)
                        .or(expected_type)
                        .unwrap_or(crate::tir::TypeTable::UNKNOWN);
                    let tir = self.reify_match_expr(match_expr, ctx, expected_type, recorded);
                    stmts.push(TirStmt::new(
                        crate::tir::TirStmtKind::Expr(tir),
                        match_expr.span,
                    ));
                    continue;
                }
                if let ast::Stmt::LabeledBlock(labeled_block) = s {
                    ctx.push_labeled_block_frame(labeled_block.label.clone(), expected_type);
                    let block = self.reify_block(&labeled_block.block, ctx, expected_type);
                    ctx.pop_labeled_block_frame();
                    stmts.push(TirStmt::new(
                        crate::tir::TirStmtKind::LabeledBlock {
                            label: labeled_block.label.clone(),
                            block,
                        },
                        labeled_block.span,
                    ));
                    continue;
                }
            }
            stmts.extend(self.reify_stmt(s, ctx));
        }
        self.mark_diverging_stmts(stmts)
    }

    /// Open the cold tail of a block at its first statement that cannot return.
    ///
    /// Marking here rather than at each synthesis site is what gives a
    /// `panic(…)` written in source the same treatment as the ones this file
    /// builds. A `return` / `break` ends a path too, but a normal exit is not a
    /// cold one, so it is left alone.
    fn mark_diverging_stmts(&self, stmts: Vec<TirStmt>) -> Vec<TirStmt> {
        let diverges =
            |s: &TirStmt| matches!(&s.kind, TirStmtKind::Expr(e) if e.type_id == TypeTable::NEVER);
        if !stmts.iter().any(diverges) {
            return stmts;
        }
        let is_marker = |s: &TirStmt| {
            matches!(&s.kind, TirStmtKind::Expr(e)
                if matches!(&e.kind, TirExprKind::Call { func, .. }
                    if func.builtin_name().as_deref() == Some("builtin::cold_path")))
        };
        let mut out: Vec<TirStmt> = Vec::with_capacity(stmts.len() + 1);
        // A marker makes the rest of its block cold, which is where `block_cut`
        // stops pricing and where `cold_outline`'s region starts, so the first
        // one in a block is the only one worth placing — and a synthesis site
        // that placed its own ahead of the statements it built keeps it.
        let mut marked = false;
        for s in stmts {
            marked = marked || is_marker(&s);
            if diverges(&s) && !marked {
                out.push(self.make_cold_path_stmt(s.span));
                marked = true;
            }
            out.push(s);
        }
        out
    }

    /// Desugar `let PAT = EXPR else { ELSE };` followed by `rest` into a
    /// two-arm `Match` on `EXPR`: arm 0 binds `PAT` and runs `rest` (the rest
    /// of the enclosing block) with the bindings in scope; arm 1 is a wildcard
    /// running the diverging `ELSE` block. Mirrors `reify_let_chain_stmts`.
    fn reify_let_else(
        &mut self,
        l: &ast::LetStmt,
        rest: &[ast::Stmt],
        block_span: crate::token::Span,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
        tail_value: bool,
    ) -> TirStmt {
        use crate::tir::{TirBlock, TirExprKind, TirMatchArm, TirPattern, TirStmtKind, TypeTable};

        let span = l.span;
        let else_ast = l
            .else_block
            .as_ref()
            .expect("reify_let_else requires an else block");
        let value_ast = l
            .value
            .as_ref()
            .expect("`let ... else` requires an initializer");

        // Mirror `resolve_let`: an annotation flows into the scrutinee as its
        // expected type (published on `let_annotated_types` during resolve).
        let annotated_type = if l.ty.is_some() {
            self.ann_let_annotated_type(l.id)
        } else {
            None
        };
        let scrutinee = self.reify_expr(value_ast, ctx, annotated_type);
        let scrutinee_type = scrutinee.type_id;

        // Reify the else block before the pattern bindings enter scope: the
        // else arm must not see them. It diverges, so its result type is Never.
        let else_block = self.reify_block(else_ast, ctx, None);
        let else_type = crate::tir::block_result_type(&self.tysys.type_table.borrow(), &else_block);
        let else_span = else_block.span;

        let tir_pattern = self.reify_pattern(&l.pattern, scrutinee_type, ctx);

        let cont_stmts =
            self.reify_positioned_stmts(rest, block_span, ctx, expected_type, tail_value);
        let cont_block = TirBlock::new(cont_stmts, block_span);
        let then_type = crate::tir::block_result_type(&self.tysys.type_table.borrow(), &cont_block);

        let match_type =
            crate::tir::agree_branch_types(&self.tysys.type_table.borrow(), then_type, else_type)
                .unwrap_or(TypeTable::UNIT);
        let then_body = TirExpr::new(TirExprKind::Block(cont_block), then_type, span);
        let else_body = TirExpr::new(TirExprKind::Block(else_block), else_type, else_span);
        let arms = vec![
            TirMatchArm {
                pattern: tir_pattern,
                guard: None,
                body: then_body,
                span,
            },
            TirMatchArm {
                pattern: TirPattern::Wildcard,
                guard: None,
                body: else_body,
                span: else_span,
            },
        ];
        TirStmt::new(
            TirStmtKind::Expr(TirExpr::new(
                TirExprKind::Match {
                    expr: Box::new(scrutinee),
                    arms,
                },
                match_type,
                span,
            )),
            span,
        )
    }

    /// Reify a statement. Dispatches on `Stmt::*`; `Let` adds a local
    /// (preserving walk-order), `For` / `While` / `Assert` consult
    /// `sem.types.desugars` to pick the right expansion path.
    pub(super) fn reify_stmt(
        &mut self,
        stmt: &ast::Stmt,
        ctx: &mut FunctionContext,
    ) -> Vec<TirStmt> {
        use crate::tir::TirStmtKind;
        match stmt {
            ast::Stmt::Expr(expr_stmt) => {
                let expr = self.reify_expr(&expr_stmt.expr, ctx, None);
                vec![TirStmt::new(TirStmtKind::Expr(expr), expr_stmt.span)]
            }
            ast::Stmt::Return(ret_stmt) => {
                let value = ret_stmt
                    .value
                    .as_ref()
                    .map(|e| self.reify_expr(e, ctx, Some(ctx.return_type)));
                vec![TirStmt::new(TirStmtKind::Return { value }, ret_stmt.span)]
            }
            ast::Stmt::TaskReturn(tr_stmt) => {
                let expected = ctx.task_return_type;
                let value = self.reify_expr(&tr_stmt.value, ctx, expected);
                vec![TirStmt::new(
                    TirStmtKind::TaskReturn { value },
                    tr_stmt.span,
                )]
            }
            ast::Stmt::Break(break_stmt) => {
                // Resolve `break label: value` against the target block's
                // expected type so a `null` / bare literal value coerces to
                // the block's result type (e.g. `Option<i32>`) rather than
                // reaching WIR as an `Option<!>` nothing inhabits.
                let break_expected = break_stmt.label.as_ref().and_then(|label| {
                    ctx.labeled_block_targets
                        .iter()
                        .rev()
                        .find(|t| &t.label == label)
                        .and_then(|t| t.expected_type)
                });
                vec![TirStmt::new(
                    TirStmtKind::Break {
                        label: break_stmt.label.clone(),
                        value: break_stmt
                            .value
                            .as_ref()
                            .map(|e| self.reify_expr(e, ctx, break_expected)),
                    },
                    break_stmt.span,
                )]
            }
            ast::Stmt::Continue(continue_stmt) => {
                // Inside a C-style `for`, `continue` must break to the
                // body label so the `update` expression runs before the
                // next iteration; only while/loop bodies use a plain
                // `Continue`. Mirror `Elaborator::resolve_continue`,
                // keyed off `ctx.for_continue_labels`.
                let stmt_kind = if let Some(body_label) = ctx.for_continue_labels.last() {
                    TirStmtKind::Break {
                        label: Some(body_label.clone()),
                        value: None,
                    }
                } else {
                    TirStmtKind::Continue
                };
                vec![TirStmt::new(stmt_kind, continue_stmt.span)]
            }
            ast::Stmt::Let(let_stmt) => vec![self.reify_let(let_stmt, ctx)],
            ast::Stmt::If(if_stmt) => self.reify_if_stmt(if_stmt, ctx),
            ast::Stmt::Match(match_expr) => {
                // Stmt-position match — `Elaborator::resolve_stmt`
                // pins `expected_type = Some(Unit)` and records the
                // result type explicitly (stmt.rs ≈84–105). Reify
                // mirrors: reify the expression at Unit, then wrap as
                // an `Expr` stmt. The reified expression's
                // `type_id` will already be `Unit` (annotate's
                // `expression_types` records the stmt-position type),
                // so the WIR builder drops each arm body's value.
                let tir = self.reify_match_expr(
                    match_expr,
                    ctx,
                    Some(crate::tir::TypeTable::UNIT),
                    crate::tir::TypeTable::UNIT,
                );
                vec![TirStmt::new(TirStmtKind::Expr(tir), match_expr.span)]
            }
            ast::Stmt::Loop(loop_stmt) => {
                // `loop { … }` — direct lowering. The
                // `for_continue_labels` save/restore mirrors
                // `Elaborator::resolve_loop`.
                let saved = std::mem::take(&mut ctx.for_continue_labels);
                let body = self.reify_block(&loop_stmt.body, ctx, None);
                ctx.for_continue_labels = saved;
                vec![TirStmt::new(TirStmtKind::Loop { body }, loop_stmt.span)]
            }
            ast::Stmt::LabeledBlock(labeled_block) => {
                // `LABEL: { … }` stmt — mirrors
                // `Elaborator::resolve_labeled_block`.
                // Push the label onto `active_labels` so a nested
                // `break LABEL` lowers against this frame, walk the
                // inner block, pop. The block result is dropped at
                // stmt position, so no `expected_type` propagates.
                ctx.push_labeled_block_frame(labeled_block.label.clone(), None);
                let block = self.reify_block(&labeled_block.block, ctx, None);
                ctx.pop_labeled_block_frame();
                vec![TirStmt::new(
                    TirStmtKind::LabeledBlock {
                        label: labeled_block.label.clone(),
                        block,
                    },
                    labeled_block.span,
                )]
            }
            ast::Stmt::While(w) => self.reify_while(w, ctx),
            ast::Stmt::For(f) => self.reify_for(f, ctx),
            ast::Stmt::Assert(assert_stmt) => self.reify_assert(assert_stmt, ctx),
            ast::Stmt::ForOf(for_of) => self.reify_for_of(for_of, ctx),
            // A local type/impl declaration emits no runtime instruction —
            // declaring a type has no effect at execution time — but its
            // own TIR (struct/newtype) is built here, in reify, from the
            // durable facts `resolve_local_struct`/`resolve_local_newtype`
            // recorded. See `reify_local_item`.
            ast::Stmt::Item(item) => {
                self.reify_local_item(item);
                vec![]
            }
            // `build_tir_from_state` skips reify for modules with syntax
            // errors, so reify never walks an `Error` placeholder.
            ast::Stmt::Error(_) => {
                unreachable!("reify does not run on modules with syntax errors")
            }
        }
    }

    /// Reify `let pat[: T] = expr;`.
    fn reify_let(&mut self, let_stmt: &ast::LetStmt, ctx: &mut FunctionContext) -> TirStmt {
        use crate::tir::{TirStmtKind, TypeTable};
        // Uninitialised `let x: T;` — the parser guarantees `ty`
        // is present. The WIR builder zero-initialises the slot;
        // reify emits a Unit placeholder as the `value` and the
        // `type_id` field carries the user-declared type. Refutable
        // patterns in this position are rejected at annotate; the
        // recovery path emits an Expr-Unit placeholder to mirror.
        let Some(ast_value) = let_stmt.value.as_ref() else {
            use crate::tir::{TirExprKind, TirStmtKind, TypeTable};
            // 7-A: same as the initialised case — read the binding's recorded
            // type (this path always binds a simple `Ident` / `MutIdent`).
            let binding_id = match &let_stmt.pattern {
                ast::Pattern::Ident { id, .. } | ast::Pattern::MutIdent { id, .. } => Some(*id),
                _ => None,
            };
            let type_id = let_stmt
                .ty
                .as_ref()
                .map(|_| {
                    binding_id
                        .and_then(|id| self.ann_local_type(id))
                        .or_else(|| self.ann_let_annotated_type(let_stmt.id))
                        .expect(
                            "uninitialised let with annotation: annotate records the type on \
                             local_types (simple binding) or let_annotated_types (destructure)",
                        )
                })
                .unwrap_or(TypeTable::UNKNOWN);
            return match &let_stmt.pattern {
                ast::Pattern::Ident {
                    id,
                    name,
                    span: binding_span,
                }
                | ast::Pattern::MutIdent {
                    id,
                    name,
                    span: binding_span,
                } => {
                    let is_mut = let_stmt.is_mut
                        || matches!(&let_stmt.pattern, ast::Pattern::MutIdent { .. });
                    let local_index =
                        ctx.add_local_at(name.clone(), type_id, is_mut, Some(*id), *binding_span);
                    let placeholder =
                        TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, let_stmt.span);
                    TirStmt::new(
                        TirStmtKind::Let {
                            name: name.clone(),
                            local_index,
                            is_mut,
                            is_reactive: let_stmt.is_reactive,
                            type_id,
                            value: placeholder,
                            skip_value_copy: false,
                        },
                        let_stmt.span,
                    )
                }
                _ => TirStmt::new(
                    TirStmtKind::Expr(TirExpr::new(
                        TirExprKind::Unit,
                        TypeTable::UNIT,
                        let_stmt.span,
                    )),
                    let_stmt.span,
                ),
            };
        };

        // 7-A (E2-thin): a simple binding's annotated type is the
        // scope-sensitive type annotate recorded as the local's type; read it
        // instead of re-resolving the annotation. Destructuring patterns bind
        // per-element, so they keep re-resolving the whole-pattern annotation.
        let simple_binding_id = match &let_stmt.pattern {
            ast::Pattern::Ident { id, .. } | ast::Pattern::MutIdent { id, .. } => Some(*id),
            _ => None,
        };
        let annotated_type = let_stmt.ty.as_ref().map(|_| {
            simple_binding_id
                .and_then(|id| self.ann_local_type(id))
                .or_else(|| self.ann_let_annotated_type(let_stmt.id))
                .expect(
                    "annotated let: annotate records the type on local_types (simple binding) \
                     or let_annotated_types (destructure)",
                )
        });
        let value = self.reify_expr(ast_value, ctx, annotated_type);
        let type_id = annotated_type.unwrap_or(value.type_id);

        match &let_stmt.pattern {
            ast::Pattern::Ident {
                id,
                name,
                span: binding_span,
            } => {
                // `let mut x = …` carries the mutability on `LetStmt`,
                // not on the `Ident` pattern.
                let is_mut = let_stmt.is_mut;
                let local_index =
                    ctx.add_local_at(name.clone(), type_id, is_mut, Some(*id), *binding_span);
                TirStmt::new(
                    TirStmtKind::Let {
                        name: name.clone(),
                        local_index,
                        is_mut,
                        is_reactive: let_stmt.is_reactive,
                        type_id,
                        value,
                        skip_value_copy: false,
                    },
                    let_stmt.span,
                )
            }
            ast::Pattern::MutIdent {
                id,
                name,
                span: binding_span,
            } => {
                let local_index =
                    ctx.add_local_at(name.clone(), type_id, true, Some(*id), *binding_span);
                TirStmt::new(
                    TirStmtKind::Let {
                        name: name.clone(),
                        local_index,
                        is_mut: true,
                        is_reactive: let_stmt.is_reactive,
                        type_id,
                        value,
                        skip_value_copy: false,
                    },
                    let_stmt.span,
                )
            }
            ast::Pattern::Wildcard => {
                // `let _ = expr;` discards. Lower as an Expr stmt.
                TirStmt::new(TirStmtKind::Expr(value), let_stmt.span)
            }
            ast::Pattern::Tuple(_, _)
            | ast::Pattern::Struct { .. }
            | ast::Pattern::Variant { .. } => {
                // Destructuring `let [a, b] = …;` / `let Point { x, y }
                // = …;` / `let Some(x) = …;`. The TIR uses
                // `TirStmtKind::LetDestructure` rather than `Let`. The
                // shared `reify_pattern` adds the sub-pattern bindings
                // to `ctx`; the value's recorded type drives the
                // pattern's per-binding type lookups.
                let pattern = self.reify_pattern(&let_stmt.pattern, type_id, ctx);
                TirStmt::new(
                    TirStmtKind::LetDestructure {
                        pattern,
                        is_mut: let_stmt.is_mut,
                        value,
                    },
                    let_stmt.span,
                )
            }
            ast::Pattern::Literal(_) | ast::Pattern::Or(_) | ast::Pattern::Range { .. } => {
                let _ = type_id;
                let _ = TypeTable::UNKNOWN;
                // `let 42 = expr;` etc. are refutable patterns and the
                // elaborator rejects them at annotate time (only
                // irrefutable patterns are valid in `let`). Hitting
                // this branch means annotate let a refutable pattern
                // through — surface the invariant violation here.
                panic!(
                    "reify_let: refutable pattern {:?} in let binding (annotate should have rejected)",
                    let_stmt.pattern
                )
            }
            // `build_tir_from_state` skips reify for modules with syntax
            // errors, so reify never walks an `Error` placeholder.
            ast::Pattern::Error(_) => {
                unreachable!("reify does not run on modules with syntax errors")
            }
        }
    }

    /// Run `f` with the default-argument override map suppressed. Mirrors
    /// `Expr::substitute_idents` leaving binder / control forms (closure,
    /// block, `if`, `match`, …) untouched on the annotate side: a reference
    /// shadowed by a binding introduced *inside* such a form must resolve to
    /// that binding, not to an outer parameter's substituted argument. No-op
    /// outside a default-argument walk. See `reify_pad_args_with_defaults`.
    fn with_defaults_suppressed<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        if self.default_arg_overrides.is_empty() {
            return f(self);
        }
        let saved = std::mem::take(&mut self.default_arg_overrides);
        let r = f(self);
        self.default_arg_overrides = saved;
        r
    }

    /// Reify an expression. Reads `sem.types.expression_types` for the
    /// type, `sem.types.coercions` for any coercion wrap,
    /// `sem.types.method_dispatch` for method calls,
    /// `sem.types.desugars` for desugar expansions, and
    /// `sem.types.generic_instantiations` for generic call /
    /// struct-literal / variant-ctor type args.
    pub(super) fn reify_expr(
        &mut self,
        expr: &ast::Expr,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
    ) -> TirExpr {
        use crate::tir::{TirExprKind, TypeTable};

        // Power-assert capture hook. See `reify_assert` /
        // `reify_with_assert_capture`.
        if let Some(actx) = ctx.reify_assert_capture_ctx.as_ref() {
            let ast_id = expr.id();
            if !actx.in_progress.contains(&ast_id)
                && let Some(&slot_idx) = actx.ast_id_to_slot.get(&ast_id)
            {
                return self.reify_with_assert_capture(slot_idx, expr, ctx, expected_type);
            }
        }

        // Compound-assign once-eval hook (see `compound_overrides`).
        if !self.compound_overrides.is_empty()
            && let Some(tir) = self.compound_overrides.get(&expr.id())
        {
            return tir.clone();
        }

        // The expression's recorded type is the source of truth for
        // `TirExpr::type_id`. Falls back to `expected_type` (or
        // `UNKNOWN` when neither is available) for AST shapes that
        // evaporated during annotate (e.g. a stmt-position match
        // whose recorder fires only at the stmt level).
        let recorded_type = self
            .ann_expression_types(expr.id())
            .or(expected_type)
            .unwrap_or(TypeTable::UNKNOWN);
        let span = expr.span();

        // Replay an i128 / u128 numeric-literal coercion: annotate
        // recorded it on `sem.types.coercions`, and unlike every other
        // `NumericLiteral` coercion (which only retags the literal's
        // type) the 128-bit structs need an explicit constructor call.
        if let Some(tir) = self.try_reify_int128_coercion(expr) {
            return tir;
        }

        match expr {
            ast::Expr::Literal(lit) => self.reify_literal(lit, recorded_type, ctx),
            ast::Expr::Block(block) => self.with_defaults_suppressed(|s| {
                let block_tir = s.reify_block_value(block, ctx, expected_type);
                TirExpr::new(TirExprKind::Block(block_tir), recorded_type, span)
            }),
            ast::Expr::Ident(ident) => self.reify_ident(ident, recorded_type, expected_type, ctx),
            ast::Expr::TupleComprehension(comp) => {
                self.reify_tuple_comprehension(comp, ctx, recorded_type)
            }
            ast::Expr::TupleLiteral(tuple_lit) => {
                // A recorded `sequence_coercions[tuple.id]` means the
                // literal builds an `Array<E>` for the target's `From`.
                if let Some(facts) = self.ann_sequence_coercions(tuple_lit.id) {
                    return self.reify_sequence_coercion(tuple_lit, facts, ctx, span);
                }
                self.reify_tuple_literal(tuple_lit, ctx, span)
            }
            ast::Expr::Cast(cast) => {
                // The cast expression's type is the resolved target type;
                // `resolve_cast` lands it on `expression_types` via the
                // `resolve_expr` wrapper. Reify is a pure read. An unresolved
                // target type (`n as NoSuchType`) is the one absence: annotate
                // reported it and records no ERROR entry, so reify carries the
                // error type on rather than reading a missing one.
                let target_type = self
                    .ann_expression_types(cast.id)
                    .unwrap_or(crate::tir::TypeTable::ERROR);
                // `i128/u128 as T` lowers to prelude calls rather than a
                // bare cast, since the 128-bit types are prelude structs:
                // floats go through the correctly rounded `as_f64` /
                // `as_f32`, integer targets truncate via `low()`, and
                // `i128 ↔ u128` reinterprets via `from_u128` / `from_i128`.
                // Must run before the target-side `try_reify_int128_cast`
                // so `i128 ↔ u128` is not mis-handled by its non-numeric
                // bare-cast fallback.
                if let Some(tir) = self.try_reify_int128_source_cast(cast, target_type, ctx) {
                    return tir;
                }
                // `expr as i128/u128` lowers to a `from_u64` / `from_i64`
                // / `from_pair` constructor call rather than a bare cast,
                // since the 128-bit types are prelude structs. Mirrors
                // `Elaborator::resolve_cast`'s int128 branch.
                if let Some(tir) = self.try_reify_int128_cast(cast, target_type, ctx) {
                    return tir;
                }
                // `expr as Ty` — emit `Cast` with the recorded target type,
                // re-typing the same integer literal operand `resolve_cast`
                // left out of the defaulted range check. annotate propagates
                // that target to a direct literal operand but not through a
                // `Neg`, so `-9e15 as i64` would otherwise emit an `i32.const`
                // that truncates before the cast widens.
                let target_is_int = self.tysys.type_table.borrow().is_integer(target_type);
                let inner = match super::expr::int_literal_cast_operand(&cast.expr) {
                    Some((lit, _, negated)) if target_is_int => {
                        let lit_tir = self.reify_literal(lit, target_type, ctx);
                        if negated {
                            TirExpr::new(
                                TirExprKind::Unary {
                                    op: crate::tir::TirUnaryOp::Neg,
                                    expr: Box::new(lit_tir),
                                },
                                target_type,
                                span,
                            )
                        } else {
                            lit_tir
                        }
                    }
                    _ => self.reify_expr(&cast.expr, ctx, None),
                };
                TirExpr::new(
                    TirExprKind::Cast {
                        expr: Box::new(inner),
                        target_type,
                    },
                    target_type,
                    span,
                )
            }
            ast::Expr::Unary(unary) => {
                // `&mut [1, 2] as List<i32>` parses as `(&mut [1, 2]) as …`,
                // and annotate coerced the literal through the borrow and typed
                // the whole unary as the target. Reify drops the wrapper to
                // match: the call site auto-borrows the `List<i32>`, where a
                // `&mut` around it would name the referent instead.
                if matches!(unary.op, ast::UnaryOp::Ref | ast::UnaryOp::MutRef)
                    && let ast::Expr::TupleLiteral(inner) = &unary.expr
                    && self.ann_sequence_coercions(inner.id).is_some()
                {
                    return self.reify_expr(&unary.expr, ctx, expected_type);
                }
                let op = ast_unary_op_to_tir(unary.op);
                // A `-<numeric literal>` operand shares the unary's type:
                // propagate the expected/recorded type so the inner literal
                // takes the right width (e.g. `-1.0` in an `f32` const body
                // must be `f32`, not the default `f64`). Other unary operands
                // are typed on their own.
                let inner_expected = if unary.op == ast::UnaryOp::Neg
                    && self.tysys.is_numeric_literal(&unary.expr)
                    && recorded_type != crate::tir::TypeTable::UNKNOWN
                {
                    Some(recorded_type)
                } else {
                    None
                };
                let inner = self.reify_expr(&unary.expr, ctx, inner_expected);
                if let Some(dispatch) = self.ann_operator_dispatch(unary.id) {
                    // Operator-trait dispatch path for `-x` / `~x` on a
                    // user type (`Neg::neg` / `BitNot::bitnot`). Mirrors the
                    // binary path: a bare `Unary` on a struct operand would
                    // be rejected by codegen (`expected i32, found (ref $T)`),
                    // so replay the recorded method call instead. Unary
                    // operators take no extra arguments.
                    let receiver = adjust_receiver_for_self_kind(
                        inner,
                        dispatch.self_kind,
                        /* is_ref_impl */ false,
                        span,
                        &self.tysys.type_table,
                    );
                    return build_tir_method_call(
                        receiver,
                        dispatch.function_ref,
                        vec![],
                        vec![],
                        dispatch.return_type,
                        span,
                    );
                }
                // Constant-fold `-literal` into a negative literal, exactly
                // as `Elaborator::resolve_unary`.
                // Without this reify emits `Unary { Neg, <pos literal> }`,
                // which lowers to `i32.sub (const 0) …` / `f64.neg …` and
                // can produce invalid modules (e.g. a negated literal that
                // only fits as the already-negative value, or a type
                // mismatch when the operand's literal type differs).
                if matches!(op, crate::tir::TirUnaryOp::Neg) {
                    match &inner.kind {
                        TirExprKind::IntLiteral { value, repr } => {
                            return TirExpr::new(
                                TirExprKind::IntLiteral {
                                    value: (*value as i64).wrapping_neg().cast_unsigned(),
                                    repr: format!("-{repr}"),
                                },
                                inner.type_id,
                                span,
                            );
                        }
                        TirExprKind::FloatLiteral { value, repr } => {
                            return TirExpr::new(
                                TirExprKind::FloatLiteral {
                                    value: -value,
                                    repr: format!("-{repr}"),
                                },
                                inner.type_id,
                                span,
                            );
                        }
                        TirExprKind::Cast {
                            expr: cast_inner,
                            target_type,
                        } if matches!(&cast_inner.kind, TirExprKind::IntLiteral { .. }) => {
                            if let TirExprKind::IntLiteral { value, repr } = &cast_inner.kind {
                                let neg_literal = TirExpr::new(
                                    TirExprKind::IntLiteral {
                                        value: (*value as i64).wrapping_neg().cast_unsigned(),
                                        repr: format!("-{repr}"),
                                    },
                                    cast_inner.type_id,
                                    span,
                                );
                                return TirExpr::new(
                                    TirExprKind::Cast {
                                        expr: Box::new(neg_literal),
                                        target_type: *target_type,
                                    },
                                    *target_type,
                                    span,
                                );
                            }
                        }
                        _ => {}
                    }
                }

                // Track address-taken locals for `&x` / `&mut x`, mirroring
                // `Elaborator::resolve_unary`. The
                // boxing pass (`lower::plan::boxing`) reads
                // `TirFunction::address_taken_locals` to retag a borrowed
                // local's declaration to its box type, so that mutation
                // through the reference (e.g. `*slot = other_fn`) writes
                // back to the original slot. Without this the local stays
                // unboxed and `&mut local` boxes a throwaway copy.
                if matches!(
                    op,
                    crate::tir::TirUnaryOp::Ref | crate::tir::TirUnaryOp::MutRef
                ) && let TirExprKind::Local { index, .. } = &inner.kind
                {
                    ctx.address_taken_locals.insert(*index);
                }
                TirExpr::new(
                    TirExprKind::Unary {
                        op,
                        expr: Box::new(inner),
                    },
                    recorded_type,
                    span,
                )
            }
            ast::Expr::MethodCall(method_call) => {
                self.reify_method_call(method_call, ctx, recorded_type)
            }
            ast::Expr::Binary(binary) => self.reify_binary(binary, ctx, recorded_type),
            ast::Expr::Call(call) => self.reify_call(call, ctx, recorded_type),
            ast::Expr::Match(match_expr) => self.with_defaults_suppressed(|s| {
                s.reify_match_expr(match_expr, ctx, expected_type, recorded_type)
            }),
            ast::Expr::StructLiteral(struct_lit) => {
                self.reify_struct_literal(struct_lit, ctx, recorded_type)
            }
            ast::Expr::Range(range) => self.reify_range(range, ctx, recorded_type),
            ast::Expr::TemplateString(template) => {
                self.reify_template_string(template, ctx, recorded_type)
            }
            ast::Expr::TaggedTemplate(tagged) => {
                self.reify_tagged_template(tagged, ctx, recorded_type)
            }
            ast::Expr::Matches(m) => self.with_defaults_suppressed(|s| s.reify_matches(m, ctx)),
            ast::Expr::CompoundAssign(compound) => {
                self.reify_compound_assign(compound, ctx, recorded_type)
            }
            ast::Expr::TryOp(qm) => self.reify_question_mark(qm, ctx, recorded_type),
            ast::Expr::Closure(closure) => self.with_defaults_suppressed(|s| {
                s.reify_closure(closure, ctx, recorded_type, expected_type)
            }),
            ast::Expr::Index(index) => self.reify_index(index, ctx, recorded_type),
            ast::Expr::ComparisonChain(chain) => self.reify_comparison_chain(chain, ctx),
            ast::Expr::StaticMethodCall(static_call) => {
                self.reify_static_method_call(static_call, ctx, recorded_type)
            }
            ast::Expr::Resume(resume) => {
                // `resume value` inside a handler method. Reify the
                // value with the function's return type as expected
                // (matches `Elaborator::resolve_resume`), then emit
                // `TirExprKind::Resume`.
                let expected = if ctx.in_handler_method {
                    Some(ctx.return_type)
                } else {
                    None
                };
                let value = self.reify_expr(&resume.value, ctx, expected);
                TirExpr::new(
                    TirExprKind::Resume {
                        value: Box::new(value),
                    },
                    crate::tir::TypeTable::UNIT,
                    span,
                )
            }
            ast::Expr::LabeledBlock(lb) => self.with_defaults_suppressed(|s| {
                // Match `Elaborator::resolve_expr`'s `LabeledBlock`
                // arm: push a `LabeledBlockTarget`
                // so any `break label: expr` inside lowers via this
                // frame, walk the inner block, pop the frame, emit
                // `TirExprKind::LabeledBlock`. The result type is the
                // recorded `expression_types[lb.id]`; annotate already
                // unified break types into it.
                // Fall back to the block's unified result type when the use
                // site supplies no expected type, so a `null` resolving only
                // from a sibling break still coerces, as a `break label: null`
                // or as the fall-through tail.
                let branch_expected = expected_type.or(Some(recorded_type));
                ctx.push_labeled_block_frame(lb.label.clone(), branch_expected);
                let tir_block = s.reify_block(&lb.block, ctx, branch_expected);
                ctx.pop_labeled_block_frame();
                TirExpr::new(
                    TirExprKind::LabeledBlock {
                        label: lb.label.clone(),
                        block: tir_block,
                        result_type: recorded_type,
                    },
                    recorded_type,
                    span,
                )
            }),
            ast::Expr::Spread(_, _) => {
                // `Spread` is only valid inside a tuple literal; the
                // elaborator panics if it sees one at top level.
                // Mirror the panic — annotate would have already
                // diagnosed a stray spread.
                panic!("reify_expr: bare Spread is invalid outside TupleLiteral")
            }
            ast::Expr::If(if_expr) => self.with_defaults_suppressed(|s| {
                s.reify_if_expr(if_expr, ctx, expected_type, recorded_type)
            }),
            ast::Expr::Assign(assign) => {
                // IndexAssign rewrite: `arr[i] = v` lowers to
                // `arr.index_assign(i, v)`. The elaborator's
                // `assign_to_target` records the resolved
                // `FunctionRef` on `index_assign_dispatch[index.id]`;
                // reify replays the same method-call shape.
                if let ast::Expr::Index(index_expr) = &assign.target
                    && let Some(dispatch) = self.ann_index_assign_dispatch(index_expr.id)
                {
                    let receiver = self.reify_expr(&index_expr.expr, ctx, None);
                    let receiver = adjust_receiver_for_self_kind(
                        receiver,
                        dispatch.self_kind,
                        false,
                        span,
                        &self.tysys.type_table,
                    );
                    let idx_expr = self.reify_expr(&index_expr.index, ctx, None);
                    let value_expr = self.reify_expr(&assign.value, ctx, None);
                    return build_tir_method_call(
                        receiver,
                        dispatch.function_ref,
                        vec![],
                        vec![
                            crate::tir::CallArg::new(idx_expr, false),
                            crate::tir::CallArg::new(value_expr, false),
                        ],
                        dispatch.return_type,
                        span,
                    );
                }
                // `target = value` — both sides walked recursively; the
                // expression's type is `Unit` (assignment is a stmt-shape
                // expression in Wado, mirroring Rust).
                let target = self.reify_expr(&assign.target, ctx, None);
                let value = self.reify_expr(&assign.value, ctx, Some(target.type_id));
                // Global-var write: `g = v` lowers to `GlobalVarSet` so
                // codegen actually mutates the global. The production
                // `assign_to_target` rewrites here too.
                if let TirExprKind::GlobalVarGet {
                    module_source,
                    name,
                } = &target.kind
                {
                    return TirExpr::new(
                        TirExprKind::GlobalVarSet {
                            module_source: module_source.clone(),
                            name: name.clone(),
                            value: Box::new(value),
                        },
                        crate::tir::TypeTable::UNIT,
                        span,
                    );
                }
                TirExpr::new(
                    TirExprKind::Assign {
                        target: Box::new(target),
                        value: Box::new(value),
                    },
                    recorded_type,
                    span,
                )
            }
            ast::Expr::FieldAccess(field_access) => {
                // The `field_index` and `field_name` on `FieldAccess`
                // TIR are positional; the elaborator looks them up from
                // the receiver's struct decl. Reify reads the same
                // info from `tysys.all_struct_fields` keyed by the
                // receiver's resolved struct name.
                let inner = self.reify_expr(&field_access.expr, ctx, None);
                let (field_index, field_name, field_type) =
                    self.lookup_struct_field_index(inner.type_id, &field_access.field);
                TirExpr::new(
                    TirExprKind::FieldAccess {
                        expr: Box::new(inner),
                        field_index,
                        field_name,
                    },
                    field_type.unwrap_or(recorded_type),
                    span,
                )
            }
            ast::Expr::WithHandler(with_expr) => {
                self.reify_with_handler(with_expr, ctx, recorded_type)
            }
            // `build_tir_from_state` skips reify for modules with syntax
            // errors, so reify never walks an `Error` placeholder.
            ast::Expr::Error(_) => {
                unreachable!("reify does not run on modules with syntax errors")
            }
        }
    }

    /// Reify an expression in condition position, the walk
    /// `resolve_condition_expr` made: a `Bool` expectation would reach the
    /// operands and propagate into `If`/`Match` branches, where it is wrong.
    fn reify_condition_expr(&mut self, expr: &ast::Expr, ctx: &mut FunctionContext) -> TirExpr {
        use crate::tir::TypeTable;

        let cond = self.reify_expr(expr, ctx, None);
        match cond.type_id {
            TypeTable::BOOL | TypeTable::UNKNOWN | TypeTable::ERROR => cond,
            type_id => unreachable!(
                "`resolve_condition_expr` rejects a non-`bool` condition, and a module holding one is never reified; got {type_id}"
            ),
        }
    }

    /// Reify a `while cond { body }` statement. Mirrors
    /// `Elaborator::resolve_while`'s `Condition::Expr` arm
    /// the loop lowers into
    /// `Loop { if !cond { break; } body }`, which is the desugar
    /// `DesugarKind::While` tags. `for_continue_labels` is saved /
    /// restored around the body walk so naked `continue` inside
    /// `while` targets this loop (not an enclosing C-style `for`).
    fn reify_while(&mut self, w: &ast::WhileStmt, ctx: &mut FunctionContext) -> Vec<TirStmt> {
        use crate::tir::{TirBlock, TirExprKind, TirStmtKind, TirUnaryOp, TypeTable};

        let span = w.span;
        let saved_continue = std::mem::take(&mut ctx.for_continue_labels);

        let stmts = match &w.condition {
            ast::Condition::Expr(cond_expr) => {
                let cond_span = cond_expr.span();
                let cond_tir = self.reify_condition_expr(cond_expr, ctx);
                let neg_cond = TirExpr::new(
                    TirExprKind::Unary {
                        op: TirUnaryOp::Not,
                        expr: Box::new(cond_tir),
                    },
                    TypeTable::BOOL,
                    cond_span,
                );
                let break_stmt = TirStmt::new(
                    TirStmtKind::Break {
                        label: None,
                        value: None,
                    },
                    span,
                );
                let if_break = TirStmt::new(
                    TirStmtKind::If {
                        condition: neg_cond,
                        then_block: TirBlock::new(vec![break_stmt], span),
                        else_block: None,
                    },
                    span,
                );
                let body_block = self.reify_block(&w.body, ctx, None);
                let mut stmts = Vec::with_capacity(1 + body_block.stmts.len());
                stmts.push(if_break);
                stmts.extend(body_block.stmts);
                stmts
            }
            ast::Condition::LetChain {
                elements,
                span: cond_span,
            } => {
                // Mirror `Elaborator::resolve_while`'s LetChain arm:
                // the else-branch unconditionally `break`s out of the loop.
                let break_stmt = TirStmt::new(
                    TirStmtKind::Break {
                        label: None,
                        value: None,
                    },
                    span,
                );
                let else_block = TirBlock::new(vec![break_stmt], *cond_span);
                ctx.enter_scope();
                let body_stmts = self.reify_let_chain_stmts(
                    elements,
                    &w.body,
                    Some(&else_block),
                    ctx,
                    None,
                    false,
                    *cond_span,
                );
                ctx.exit_scope();
                body_stmts
            }
        };

        ctx.for_continue_labels = saved_continue;
        vec![TirStmt::new(
            TirStmtKind::Loop {
                body: TirBlock::new(stmts, span),
            },
            span,
        )]
    }

    /// Reify hook for a power-assert-flagged sub-expression: allocates the
    /// slot's locals and returns what the surrounding reify splices in.
    /// Mirrors [`super::Elaborator::resolve_with_assert_capture`].
    fn reify_with_assert_capture(
        &mut self,
        slot_idx: usize,
        expr: &ast::Expr,
        ctx: &mut FunctionContext,
        expected_type: Option<crate::tir::TypeId>,
    ) -> TirExpr {
        use crate::tir::{TirExprKind, TirStmtKind};

        let ast_id = expr.id();
        ctx.reify_assert_capture_ctx
            .as_mut()
            .expect("reify_assert_capture_ctx present (guarded by caller)")
            .in_progress
            .insert(ast_id);

        let resolved = self.reify_expr(expr, ctx, expected_type);

        ctx.reify_assert_capture_ctx
            .as_mut()
            .expect("reify_assert_capture_ctx survives recursive reify")
            .in_progress
            .shift_remove(&ast_id);

        let type_id = resolved.type_id;
        let cap_span = resolved.span;
        let (cap_name, conditional, is_place, hoisted) = {
            let slot = &ctx
                .reify_assert_capture_ctx
                .as_ref()
                .expect("reify_assert_capture_ctx survives recursive reify")
                .slots[slot_idx];
            (
                slot.name.clone(),
                slot.conditional,
                slot.is_place,
                slot.hoisted,
            )
        };

        // Re-read rather than bind: straight-line code makes the read exact,
        // and a binding would keep an aggregate live across the assert.
        if is_place && !conditional {
            let cap_ctx = ctx
                .reify_assert_capture_ctx
                .as_mut()
                .expect("reify_assert_capture_ctx survives recursive reify");
            cap_ctx.slots[slot_idx].emitted = true;
            cap_ctx.slots[slot_idx].type_id = Some(type_id);
            cap_ctx.slots[slot_idx].place_expr = Some(resolved.clone());
            return resolved;
        }

        // `defining_ast_id = None` keeps synthetic locals out of
        // `local_symbols` (LSP hover / go-to-def).
        let local_index = ctx.add_local(cap_name.clone(), type_id, !hoisted, None);
        let seen_local_index = conditional.then(|| {
            ctx.add_local(
                super::assert::seen_local_name(&cap_name),
                crate::tir::TypeTable::BOOL,
                true,
                None,
            )
        });

        let local_ref = TirExpr::new(
            TirExprKind::Local {
                index: local_index,
                name: cap_name.clone(),
            },
            type_id,
            cap_span,
        );

        // A hoisted slot binds ahead of the condition, its scope reaching the
        // failure branch too — sound because all that precedes it is bound too.
        if hoisted {
            assert!(
                seen_local_index.is_none(),
                "a conditional slot is never hoisted"
            );
            let binding = TirStmt::new(
                TirStmtKind::Let {
                    name: cap_name,
                    local_index,
                    is_mut: false,
                    is_reactive: false,
                    type_id,
                    value: resolved,
                    skip_value_copy: false,
                },
                cap_span,
            );
            let cap_ctx = ctx
                .reify_assert_capture_ctx
                .as_mut()
                .expect("reify_assert_capture_ctx survives recursive reify");
            cap_ctx.slots[slot_idx].emitted = true;
            cap_ctx.slots[slot_idx].local_index = Some(local_index);
            cap_ctx.slots[slot_idx].type_id = Some(type_id);
            cap_ctx.emitted_lets.push(binding);
            return local_ref;
        }

        // Otherwise the capture sits where the operand does, moving no
        // evaluation; until it runs, the local holds its Wasm default.
        let (decls, spliced) = if let Some(seen_index) = seen_local_index {
            let decls = vec![TirStmt::new(
                TirStmtKind::Let {
                    name: super::assert::seen_local_name(&cap_name),
                    local_index: seen_index,
                    is_mut: true,
                    is_reactive: false,
                    type_id: crate::tir::TypeTable::BOOL,
                    value: TirExpr::new(
                        TirExprKind::BoolLiteral(false),
                        crate::tir::TypeTable::BOOL,
                        cap_span,
                    ),
                    skip_value_copy: true,
                },
                cap_span,
            )];
            let capture_block = crate::tir::TirBlock::new(
                vec![
                    assign_stmt(local_ref.clone(), resolved, cap_span),
                    assign_stmt(
                        TirExpr::new(
                            TirExprKind::Local {
                                index: seen_index,
                                name: super::assert::seen_local_name(&cap_name),
                            },
                            crate::tir::TypeTable::BOOL,
                            cap_span,
                        ),
                        TirExpr::new(
                            TirExprKind::BoolLiteral(true),
                            crate::tir::TypeTable::BOOL,
                            cap_span,
                        ),
                        cap_span,
                    ),
                    TirStmt::new(TirStmtKind::Expr(local_ref), cap_span),
                ],
                cap_span,
            );
            (
                decls,
                TirExpr::new(TirExprKind::Block(capture_block), type_id, cap_span),
            )
        } else {
            let capture_block = crate::tir::TirBlock::new(
                vec![
                    assign_stmt(local_ref.clone(), resolved, cap_span),
                    TirStmt::new(TirStmtKind::Expr(local_ref), cap_span),
                ],
                cap_span,
            );
            (
                Vec::new(),
                TirExpr::new(TirExprKind::Block(capture_block), type_id, cap_span),
            )
        };

        let cap_ctx = ctx
            .reify_assert_capture_ctx
            .as_mut()
            .expect("reify_assert_capture_ctx survives recursive reify");
        cap_ctx.slots[slot_idx].emitted = true;
        cap_ctx.slots[slot_idx].local_index = Some(local_index);
        cap_ctx.slots[slot_idx].type_id = Some(type_id);
        cap_ctx.slots[slot_idx].seen_local_index = seen_local_index;
        cap_ctx.emitted_lets.extend(decls);

        spliced
    }

    /// Reify `assert cond[, msg];` into the power-assert expansion, from the
    /// slots annotate recorded in [`super::sem::types::AssertCaptureInfo`].
    /// Mirrors [`super::Elaborator::desugar_assert`].
    /// Build a `builtin::cold_path()` marker statement for a compiler-synthesized
    /// cold branch (an `assert` failure, a `?` error propagation, …). Codegen
    /// then hints the enclosing branch unlikely and the inliner skips the branch.
    fn make_cold_path_stmt(&self, span: crate::token::Span) -> TirStmt {
        use crate::synthesis::common::builtin_call;
        use crate::tir::{TirStmtKind, TypeTable};
        let call = builtin_call("cold_path", Vec::new(), TypeTable::UNIT);
        TirStmt::new(TirStmtKind::Expr(call), span)
    }

    /// A conditional slot's failure-message text, chosen in the cold branch so
    /// an unreached slot says so instead of quoting its zero value.
    #[allow(clippy::too_many_arguments)]
    fn assert_slot_text_let(
        &mut self,
        render_index: u32,
        render_name: String,
        string_type: TypeId,
        seen_index: u32,
        seen_name: String,
        value_ref: TirExpr,
        inspect_spec: Option<crate::format_spec::TemplateFormatSpec>,
        span: crate::token::Span,
    ) -> TirStmt {
        use crate::tir::TirTemplatePart;

        let rendered = TirExpr::new(
            TirExprKind::TemplateString {
                parts: vec![TirTemplatePart::Interpolation {
                    expr: Box::new(value_ref),
                    format_spec: inspect_spec,
                }],
            },
            string_type,
            span,
        );
        let marker = TirExpr::new(
            TirExprKind::StringLiteral(super::assert::NOT_EVALUATED.to_string()),
            string_type,
            span,
        );
        let choice = TirExpr::new(
            TirExprKind::If {
                condition: Box::new(TirExpr::new(
                    TirExprKind::Local {
                        index: seen_index,
                        name: seen_name,
                    },
                    TypeTable::BOOL,
                    span,
                )),
                then_branch: TirBlock::new(
                    vec![TirStmt::new(TirStmtKind::Expr(rendered), span)],
                    span,
                ),
                else_branch: Some(TirBlock::new(
                    vec![TirStmt::new(TirStmtKind::Expr(marker), span)],
                    span,
                )),
            },
            string_type,
            span,
        );
        TirStmt::new(
            TirStmtKind::Let {
                name: render_name,
                local_index: render_index,
                is_mut: false,
                is_reactive: false,
                type_id: string_type,
                value: choice,
                skip_value_copy: false,
            },
            span,
        )
    }

    fn reify_assert(
        &mut self,
        assert_stmt: &ast::AssertStmt,
        ctx: &mut FunctionContext,
    ) -> Vec<TirStmt> {
        use crate::tir::{
            CallArg, FunctionRef, TirBlock, TirExprKind, TirStmtKind, TirTemplatePart, TirUnaryOp,
            TypeTable,
        };

        let span = assert_stmt.span;

        // Always install the context: an empty `ast_id_to_slot` map
        // intercepts nothing, and the hook is a single Option check.
        let info = self.ann_assert_captures(assert_stmt.id);
        let (slot_facts, ast_id_to_slot): (
            Vec<(String, bool, bool, bool)>,
            IndexMap<AstId, usize>,
        ) = if let Some(info) = info.as_ref() {
            let mut facts: Vec<(String, bool, bool, bool)> = Vec::with_capacity(info.slots.len());
            let mut map: IndexMap<AstId, usize> = IndexMap::default();
            for (i, s) in info.slots.iter().enumerate() {
                facts.push((
                    s.capture_label.clone(),
                    s.conditional,
                    s.is_place,
                    s.hoisted,
                ));
                map.insert(s.ast_id, i);
            }
            (facts, map)
        } else {
            (Vec::new(), IndexMap::default())
        };

        ctx.enter_scope();

        ctx.reify_assert_capture_ctx = Some(ReifyAssertCaptureContext {
            slots: slot_facts
                .iter()
                .enumerate()
                .map(
                    |(i, (label, conditional, is_place, hoisted))| ReifyAssertSlot {
                        name: format!("__v{i}"),
                        label: label.clone(),
                        emitted: false,
                        local_index: None,
                        type_id: None,
                        conditional: *conditional,
                        is_place: *is_place,
                        hoisted: *hoisted,
                        place_expr: None,
                        seen_local_index: None,
                    },
                )
                .collect(),
            ast_id_to_slot,
            in_progress: IndexSet::default(),
            emitted_lets: Vec::new(),
        });

        let cond_tir = self.reify_condition_expr(&assert_stmt.condition, ctx);

        let actx = ctx
            .reify_assert_capture_ctx
            .take()
            .expect("reify_assert_capture_ctx survives condition reify");

        let mut inner_stmts: Vec<TirStmt> = Vec::with_capacity(actx.emitted_lets.len() + 2);
        inner_stmts.extend(actx.emitted_lets);

        let cond_type = cond_tir.type_id;
        let cond_name = "__cond".to_string();
        let cond_local_index = ctx.add_local(cond_name.clone(), cond_type, false, None);
        inner_stmts.push(TirStmt::new(
            TirStmtKind::Let {
                name: cond_name.clone(),
                local_index: cond_local_index,
                is_mut: false,
                is_reactive: false,
                type_id: cond_type,
                value: cond_tir,
                skip_value_copy: false,
            },
            span,
        ));

        let string_type = self
            .tysys
            .type_table
            .borrow_mut()
            .make_compiler_struct(crate::compiler_item::CompilerItem::String);
        // Allocated for every conditional slot, not just the emitted ones, so
        // annotate's index accounting stays in lockstep.
        let render_local_of: IndexMap<usize, u32> = actx
            .slots
            .iter()
            .enumerate()
            .filter(|(_, slot)| slot.conditional)
            .map(|(i, slot)| {
                let name = super::assert::render_local_name(&slot.name);
                (i, ctx.add_local(name, string_type, false, None))
            })
            .collect();

        let cond_ref = TirExpr::new(
            TirExprKind::Local {
                index: cond_local_index,
                name: cond_name,
            },
            cond_type,
            span,
        );
        let neg_cond = TirExpr::new(
            TirExprKind::Unary {
                op: TirUnaryOp::Not,
                expr: Box::new(cond_ref),
            },
            TypeTable::BOOL,
            span,
        );

        // Header + `condition: <source>` + one `<label>: <text>` line per
        // emitted slot.
        let line = span.line as u64;
        let mut parts: Vec<TirTemplatePart> = vec![
            TirTemplatePart::Literal("Assertion failed in ".to_string()),
            TirTemplatePart::Interpolation {
                expr: Box::new(TirExpr::new(
                    TirExprKind::StringLiteral(ctx.function_name.clone()),
                    string_type,
                    span,
                )),
                format_spec: None,
            },
            TirTemplatePart::Literal(" at ".to_string()),
            TirTemplatePart::Interpolation {
                expr: Box::new(TirExpr::new(
                    TirExprKind::StringLiteral(self.current_module_source.to_string()),
                    string_type,
                    span,
                )),
                format_spec: None,
            },
            TirTemplatePart::Literal(":".to_string()),
            TirTemplatePart::Interpolation {
                expr: Box::new(TirExpr::new(
                    TirExprKind::IntLiteral {
                        value: line,
                        repr: line.to_string(),
                    },
                    TypeTable::I32,
                    span,
                )),
                format_spec: None,
            },
        ];
        if let Some(msg) = &assert_stmt.message {
            parts.push(TirTemplatePart::Literal(": ".to_string()));
            let msg_tir = self.reify_expr(msg, ctx, None);
            parts.push(TirTemplatePart::Interpolation {
                expr: Box::new(msg_tir),
                format_spec: None,
            });
        }

        let condition_source = info.as_ref().map_or_else(
            || crate::unparse::unparse_expr_source(&assert_stmt.condition),
            |info| info.condition_source.clone(),
        );
        parts.push(TirTemplatePart::Literal(format!(
            "\ncondition: {condition_source}\n"
        )));

        let mut text_lets: Vec<TirStmt> = Vec::new();
        for (slot_idx, slot) in actx.slots.iter().enumerate() {
            if !slot.emitted {
                continue;
            }
            let Some(type_id) = slot.type_id else {
                continue;
            };
            parts.push(TirTemplatePart::Literal(format!("{}: ", slot.label)));
            let local_ref = match (&slot.place_expr, slot.local_index) {
                (Some(place), _) => place.clone(),
                (None, Some(local_index)) => TirExpr::new(
                    TirExprKind::Local {
                        index: local_index,
                        name: slot.name.clone(),
                    },
                    type_id,
                    span,
                ),
                (None, None) => continue,
            };
            let inspect_spec = Some(crate::format_spec::TemplateFormatSpec::of_kind(
                crate::format_spec::FormatKind::Inspect,
            ));
            match slot.seen_local_index {
                Some(seen_index) => {
                    let render_index = render_local_of[&slot_idx];
                    let render_name = super::assert::render_local_name(&slot.name);
                    text_lets.push(self.assert_slot_text_let(
                        render_index,
                        render_name.clone(),
                        string_type,
                        seen_index,
                        super::assert::seen_local_name(&slot.name),
                        local_ref,
                        inspect_spec,
                        span,
                    ));
                    parts.push(TirTemplatePart::Interpolation {
                        expr: Box::new(TirExpr::new(
                            TirExprKind::Local {
                                index: render_index,
                                name: render_name,
                            },
                            string_type,
                            span,
                        )),
                        format_spec: None,
                    });
                }
                None => parts.push(TirTemplatePart::Interpolation {
                    expr: Box::new(local_ref),
                    format_spec: inspect_spec,
                }),
            }
            parts.push(TirTemplatePart::Literal("\n".to_string()));
        }

        let template_tir = TirExpr::new(TirExprKind::TemplateString { parts }, string_type, span);

        // Emit `core:rt::assert_failed`, not `panic`: a distinct callee
        // lets `-f bare-asserts` (see `lower::bare_asserts`) replace assertion
        // failures with a bare trap, dropping this diagnostic without touching
        // explicit `panic(...)` calls. It behaves identically to `panic`.
        let assert_failed_module_source = self.interner.borrow_mut().core("rt");
        let panic_call = TirExpr::new(
            TirExprKind::Call {
                func: Box::new(FunctionRef {
                    module_source: assert_failed_module_source,
                    name: "assert_failed".to_string(),
                    monomorph_info: None,
                    method_info: None,
                }),
                type_args: Vec::new(),
                args: vec![CallArg::new(template_tir, false)],
                has_receiver: false,
            },
            TypeTable::NEVER,
            span,
        );

        let mut then_stmts = Vec::with_capacity(text_lets.len() + 2);
        then_stmts.push(self.make_cold_path_stmt(span));
        then_stmts.extend(text_lets);
        then_stmts.push(TirStmt::new(TirStmtKind::Expr(panic_call), span));
        let then_block = TirBlock::new(then_stmts, span);
        inner_stmts.push(TirStmt::new(
            TirStmtKind::If {
                condition: neg_cond,
                then_block,
                else_block: None,
            },
            span,
        ));

        ctx.exit_scope();

        // The `__assert_N:` label advances `next_assert_id` in lockstep with
        // annotate's own walk.
        let assert_serial = ctx.next_assert_id;
        ctx.next_assert_id += 1;
        vec![TirStmt::new(
            TirStmtKind::LabeledBlock {
                label: format!("$assert_{assert_serial}"),
                block: TirBlock::new(inner_stmts, span),
            },
            span,
        )]
    }

    /// Reify a `for x of expr { body }` loop, picking the expansion path from
    /// the `DesugarKind` tag on `for_of.id`: `ForOfIterator` emits
    /// `match next() { Some(v) => body, _ => break }`, `ForOfTuple`
    /// compile-time-unrolls into per-element labelled blocks, and
    /// `ForOfVariadic` defers to the monomorphizer via a `VariadicForOf` node.
    fn reify_for_of(&mut self, for_of: &ast::ForOfStmt, ctx: &mut FunctionContext) -> Vec<TirStmt> {
        use crate::tir::{TirExprKind, TirStmtKind, TypeTable};

        match self.ann_desugars(for_of.id) {
            Some(super::sem::types::DesugarKind::ForOfTuple) => {
                self.reify_tuple_for_of(for_of, ctx)
            }
            Some(super::sem::types::DesugarKind::ForOfVariadic) => {
                self.reify_variadic_for_of(for_of, ctx)
            }
            Some(super::sem::types::DesugarKind::ForOfIterator) | None => {
                let Some(info) = self.ann_for_of_iterator(for_of.id) else {
                    return vec![TirStmt::new(
                        TirStmtKind::Expr(TirExpr::new(
                            TirExprKind::Unit,
                            TypeTable::UNIT,
                            for_of.span,
                        )),
                        for_of.span,
                    )];
                };
                self.reify_iterator_for_of(for_of, ctx, info)
            }
            _ => unreachable!("for_of carries one of the three ForOf* desugar tags"),
        }
    }

    /// `IntoIterator` path of for-of (extracted from
    /// `reify_for_of` for readability). The `for_of_iterator` record
    /// carries the resolved `into_iter` / `next` `FunctionRef`s.
    fn reify_iterator_for_of(
        &mut self,
        for_of: &ast::ForOfStmt,
        ctx: &mut FunctionContext,
        info: super::sem::types::ForOfIteratorInfo,
    ) -> Vec<TirStmt> {
        use crate::tir::{
            CallArg, ResolvedType, TirBlock, TirExprKind, TirMatchArm, TirPattern, TirStmtKind,
            TypeTable,
        };

        let span = for_of.span;
        let saved_continue = std::mem::take(&mut ctx.for_continue_labels);
        let unique_id = ctx.next_local;
        let iter_var = format!("__iter_{unique_id}");
        let label = format!("$for_of_{unique_id}");

        let into_iter_receiver = self.reify_expr(&for_of.iterable, ctx, None);
        let into_iter_receiver = adjust_receiver_for_self_kind(
            into_iter_receiver,
            info.into_iter_self_kind,
            info.into_iter_is_ref_impl,
            span,
            &self.tysys.type_table,
        );
        let iter_type = info.iter_type;
        let into_iter_call = build_tir_method_call(
            into_iter_receiver,
            info.into_iter.clone(),
            vec![],
            vec![],
            iter_type,
            span,
        );

        let iter_local_index =
            ctx.add_local(iter_var.clone(), iter_type, /* is_mut */ true, None);
        let iter_let = TirStmt::new(
            TirStmtKind::Let {
                name: iter_var.clone(),
                local_index: iter_local_index,
                is_mut: true,
                is_reactive: false,
                type_id: iter_type,
                value: into_iter_call,
                skip_value_copy: false,
            },
            span,
        );

        ctx.active_labels.push(label.clone());

        let iter_local_ref = TirExpr::new(
            TirExprKind::Local {
                index: iter_local_index,
                name: iter_var,
            },
            iter_type,
            span,
        );
        let next_receiver = adjust_receiver_for_self_kind(
            iter_local_ref,
            info.next_self_kind,
            info.next_is_ref_impl,
            span,
            &self.tysys.type_table,
        );
        let option_type = self
            .tysys
            .type_table
            .borrow_mut()
            .make_option(info.item_type);
        let next_call = build_tir_method_call(
            next_receiver,
            info.next.clone(),
            vec![],
            vec![],
            option_type,
            span,
        );

        let some_case_name = self
            .tysys
            .type_table
            .borrow()
            .compiler_variant_case_name(crate::compiler_item::CompilerItem::OptionSome)
            .to_string();

        ctx.enter_scope();
        let binding_pattern = self.reify_pattern(&for_of.binding, info.item_type, ctx);
        let body_block = self.reify_block(&for_of.body, ctx, None);
        ctx.exit_scope();

        let some_pattern = TirPattern::Variant {
            enum_type: option_type,
            variant_name: some_case_name,
            bindings: vec![binding_pattern],
            payload_type: info.item_type,
        };

        let body_type = match &body_block.stmts.last() {
            Some(stmt) => match &stmt.kind {
                TirStmtKind::Expr(e) => e.type_id,
                _ => TypeTable::UNIT,
            },
            None => TypeTable::UNIT,
        };
        let some_body = TirExpr::new(TirExprKind::Block(body_block), body_type, span);

        let break_block = TirBlock::new(
            vec![TirStmt::new(
                TirStmtKind::Break {
                    label: None,
                    value: None,
                },
                span,
            )],
            span,
        );
        let break_body = TirExpr::new(TirExprKind::Block(break_block), TypeTable::UNIT, span);

        let match_type = crate::tir::agree_branch_types(
            &self.tysys.type_table.borrow(),
            body_type,
            TypeTable::UNIT,
        )
        .unwrap_or(TypeTable::UNIT);
        let arms = vec![
            TirMatchArm {
                pattern: some_pattern,
                guard: None,
                body: some_body,
                span,
            },
            TirMatchArm {
                pattern: TirPattern::Wildcard,
                guard: None,
                body: break_body,
                span,
            },
        ];
        let match_expr = TirExpr::new(
            TirExprKind::Match {
                expr: Box::new(next_call),
                arms,
            },
            match_type,
            span,
        );
        let loop_body = TirBlock::new(
            vec![TirStmt::new(TirStmtKind::Expr(match_expr), span)],
            span,
        );
        let loop_tir = TirStmt::new(TirStmtKind::Loop { body: loop_body }, span);

        ctx.active_labels.pop();
        ctx.for_continue_labels = saved_continue;

        let _ = CallArg::new(
            TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, span),
            false,
        );
        let _ = ResolvedType::Unit;

        vec![TirStmt::new(
            TirStmtKind::LabeledBlock {
                label,
                block: TirBlock::new(vec![iter_let, loop_tir], span),
            },
            span,
        )]
    }

    /// Compile-time-unroll a tuple for-of into per-element
    /// labelled blocks. Mirrors `Elaborator::resolve_tuple_for_of`
    /// Handles `.enumerate()` unwrap on the AST
    /// receiver so each iteration sees `[i, element]`.
    fn reify_tuple_for_of(
        &mut self,
        for_of: &ast::ForOfStmt,
        ctx: &mut FunctionContext,
    ) -> Vec<TirStmt> {
        use crate::tir::{TirBlock, TirExprKind, TirStmtKind, TypeTable};

        let span = for_of.span;
        let unique_id = ctx.next_local;

        let (actual_iterable, is_enumerate) = match &for_of.iterable {
            ast::Expr::MethodCall(mc) if mc.method == "enumerate" && mc.args.is_empty() => {
                (&mc.receiver, true)
            }
            other => (other, false),
        };
        let iterable = self.reify_expr(actual_iterable, ctx, None);
        let tuple_type_id = iterable.type_id;
        // Look through a `&`/`&mut` wrapper: `for v of &tuple` binds each
        // element by reference (`&T_k`), mirroring `resolve_tuple_for_of`.
        let (elems, by_ref): (Vec<TypeId>, bool) = self
            .tysys
            .type_table
            .borrow()
            .as_tuple_through_ref(tuple_type_id)
            .unwrap_or_default();

        let temp_name = format!("__tuple_{unique_id}");
        let temp_local = ctx.add_local(temp_name.clone(), tuple_type_id, false, None);
        let temp_let = TirStmt::new(
            TirStmtKind::Let {
                name: temp_name.clone(),
                local_index: temp_local,
                is_mut: false,
                is_reactive: false,
                type_id: tuple_type_id,
                value: iterable,
                skip_value_copy: false,
            },
            span,
        );

        let mut outer_stmts = vec![temp_let];

        // Consume this for-of's overlays for the current instantiation.
        // Annotate pushed one per-element overlay set per instantiation in
        // walk order; the visit counter selects the matching one (a nested
        // inner for-of is instantiated once per outer element). Each
        // element's overlay is pushed onto `tuple_overlay_stack` while its
        // binding and body are reified so the `ann_*` accessors see the
        // right per-element facts instead of the truncated base maps.
        // Borrowed, not copied: an overlay is 20 maps and reify only reads it.
        let instantiation: &'a [super::sem::types::BodyFacts] = {
            let sem: &'a ModuleSemantics = self.sem;
            let for_of_key = for_of.id;
            let visit = self.tuple_overlay_visits.entry(for_of_key).or_insert(0);
            let k = *visit;
            *visit += 1;
            sem.types
                .tuple_overlays
                .get(&for_of_key)
                .and_then(|insts| insts.get(k))
                .map_or(&[], Vec::as_slice)
        };

        for (i, &elem_type) in elems.iter().enumerate() {
            ctx.enter_scope();
            if let Some(overlay) = instantiation.get(i) {
                self.tuple_overlay_stack.push(overlay);
            }

            let temp_ref = TirExpr::new(
                TirExprKind::Local {
                    index: temp_local,
                    name: temp_name.clone(),
                },
                tuple_type_id,
                span,
            );
            let field_access = TirExpr::new(
                TirExprKind::FieldAccess {
                    expr: Box::new(temp_ref),
                    field_index: i as u32,
                    field_name: i.to_string(),
                },
                elem_type,
                span,
            );

            // By reference (`for v of &tuple`), bind `&T_k` to a fresh copy of
            // the field, matching `for v of &list` refiter semantics.
            let (bind_elem_type, bind_value) = self
                .tysys
                .type_table
                .borrow_mut()
                .tuple_element_binding(field_access, elem_type, by_ref, span);

            let mut block_stmts = Vec::new();

            if is_enumerate {
                let i32_type = TypeTable::I32;
                let index_literal = TirExpr::new(
                    TirExprKind::IntLiteral {
                        value: i as u64,
                        repr: i.to_string(),
                    },
                    i32_type,
                    span,
                );
                let enum_tuple_type = self
                    .tysys
                    .type_table
                    .borrow_mut()
                    .make_tuple(vec![i32_type, bind_elem_type]);
                let enum_tuple = TirExpr::new(
                    TirExprKind::TupleLiteral {
                        elements: vec![index_literal, bind_value],
                    },
                    enum_tuple_type,
                    span,
                );
                let tir_pattern = self.reify_pattern(&for_of.binding, enum_tuple_type, ctx);
                block_stmts.push(TirStmt::new(
                    TirStmtKind::LetDestructure {
                        pattern: tir_pattern,
                        is_mut: for_of.is_mut,
                        value: enum_tuple,
                    },
                    span,
                ));
            } else {
                match &for_of.binding {
                    ast::Pattern::Ident {
                        id,
                        name,
                        span: binding_span,
                    }
                    | ast::Pattern::MutIdent {
                        id,
                        name,
                        span: binding_span,
                    } => {
                        let is_mut = for_of.is_mut
                            || matches!(&for_of.binding, ast::Pattern::MutIdent { .. });
                        let local_index = ctx.add_local_at(
                            name.clone(),
                            bind_elem_type,
                            is_mut,
                            Some(*id),
                            *binding_span,
                        );
                        block_stmts.push(TirStmt::new(
                            TirStmtKind::Let {
                                name: name.clone(),
                                local_index,
                                is_mut,
                                is_reactive: false,
                                type_id: bind_elem_type,
                                value: bind_value,
                                skip_value_copy: false,
                            },
                            span,
                        ));
                    }
                    ast::Pattern::Tuple(_, _) | ast::Pattern::Struct { .. } => {
                        let tir_pattern = self.reify_pattern(&for_of.binding, bind_elem_type, ctx);
                        block_stmts.push(TirStmt::new(
                            TirStmtKind::LetDestructure {
                                pattern: tir_pattern,
                                is_mut: for_of.is_mut,
                                value: bind_value,
                            },
                            span,
                        ));
                    }
                    ast::Pattern::Wildcard => {
                        block_stmts.push(TirStmt::new(TirStmtKind::Expr(bind_value), span));
                    }
                    _ => {
                        // Annotate diagnosed; emit nothing.
                    }
                }
            }

            let body = self.reify_block(&for_of.body, ctx, None);
            block_stmts.extend(body.stmts);

            if instantiation.get(i).is_some() {
                self.tuple_overlay_stack.pop();
            }
            ctx.exit_scope();

            outer_stmts.push(TirStmt::new(
                TirStmtKind::LabeledBlock {
                    label: format!("$tuple_iter_{unique_id}_{i}"),
                    block: TirBlock::new(block_stmts, span),
                },
                span,
            ));
        }

        let label = format!("$tuple_for_of_{unique_id}");
        ctx.active_labels.push(label.clone());
        let result = vec![TirStmt::new(
            TirStmtKind::LabeledBlock {
                label,
                block: TirBlock::new(outer_stmts, span),
            },
            span,
        )];
        ctx.active_labels.pop();
        result
    }

    /// Emit a deferred `VariadicForOf` TIR node for tuples whose
    /// element types contain `TypePack`. The monomorphizer expands
    /// this after `TypePack` substitution. Mirrors
    /// `Elaborator::resolve_variadic_for_of`.
    fn reify_variadic_for_of(
        &mut self,
        for_of: &ast::ForOfStmt,
        ctx: &mut FunctionContext,
    ) -> Vec<TirStmt> {
        use crate::tir::{ResolvedType, TirExprKind, TirStmtKind, TypeTable};

        let span = for_of.span;
        let (actual_iterable, is_enumerate) = match &for_of.iterable {
            ast::Expr::MethodCall(mc) if mc.method == "enumerate" && mc.args.is_empty() => {
                (&mc.receiver, true)
            }
            other => (other, false),
        };
        let iterable = self.reify_expr(actual_iterable, ctx, None);
        let unique_id = ctx.next_local;

        // Look through a `&`/`&mut` wrapper: `for v of &[..T]` binds `&T_k`.
        // `by_ref` and the element type are derived from a single lookup so the
        // two cannot drift.
        let (inner, by_ref) = {
            let type_table = self.tysys.type_table.borrow();
            match type_table.as_tuple_through_ref(iterable.type_id) {
                Some((elems, by_ref)) => {
                    let inner = elems
                        .iter()
                        .find(|e| matches!(type_table.get(**e), ResolvedType::TypePack { .. }))
                        .or_else(|| elems.first())
                        .copied()
                        .unwrap_or(TypeTable::UNKNOWN);
                    // A mapped pack (`[..Case<T, P>]`) binds the loop variable to
                    // the mapped element, not the pack itself.
                    let inner = match type_table.get(inner) {
                        ResolvedType::TypePack {
                            mapped_elem: Some(elem),
                            ..
                        } => *elem,
                        _ => inner,
                    };
                    (inner, by_ref)
                }
                None => (TypeTable::UNKNOWN, false),
            }
        };
        let bound_type = if by_ref {
            self.tysys.type_table.borrow_mut().make_ref(inner)
        } else {
            inner
        };
        // Expansion supplies the index literal per unrolled element.
        let binding_type = if is_enumerate {
            self.tysys
                .type_table
                .borrow_mut()
                .make_tuple(vec![TypeTable::I32, bound_type])
        } else {
            bound_type
        };

        let (binding_name, binding_id, binding_name_span) = match &for_of.binding {
            ast::Pattern::Ident {
                id,
                name,
                span: name_span,
            } => (name.clone(), Some(*id), *name_span),
            ast::Pattern::Tuple(..) => (
                format!("__pattern_temp_{unique_id}"),
                None,
                crate::token::Span::default(),
            ),
            _ => {
                return vec![TirStmt::new(TirStmtKind::Expr(iterable), span)];
            }
        };

        let is_mut = for_of.is_mut;
        ctx.enter_scope();
        let binding_local = ctx.add_local_at(
            binding_name.clone(),
            binding_type,
            is_mut,
            binding_id,
            binding_name_span,
        );

        // Destructured binding (`for let [a, b] of …`): bind each inner
        // pattern variable to its element type and prepend a field-access
        // `Let` reading it from the synthetic pair temp, mirroring
        // `resolve_variadic_for_of`. Without this the inner
        // names (`a`, `b`) never enter scope, so the body resolves them to
        // `Unknown` — e.g. `a != b` in the variadic `Eq for [..T]` impl
        // dispatches to a nonexistent `unknown^Eq::eq`.
        let mut destruct_stmts: Vec<TirStmt> = Vec::new();
        if let ast::Pattern::Tuple(tp, _) = &for_of.binding {
            let inner_elems = self
                .tysys
                .type_table
                .borrow()
                .as_tuple(binding_type)
                .unwrap_or_else(|| vec![binding_type]);
            for (i, pat_elem) in tp.iter().enumerate() {
                if let ast::Pattern::Ident {
                    id,
                    name,
                    span: elem_span,
                    ..
                } = pat_elem
                {
                    let elem_type = inner_elems.get(i).copied().unwrap_or(TypeTable::UNKNOWN);
                    let local_idx =
                        ctx.add_local_at(name.clone(), elem_type, is_mut, Some(*id), *elem_span);
                    let field_access = TirExpr::new(
                        TirExprKind::FieldAccess {
                            expr: Box::new(TirExpr::new(
                                TirExprKind::Local {
                                    index: binding_local,
                                    name: binding_name.clone(),
                                },
                                binding_type,
                                span,
                            )),
                            field_index: i as u32,
                            field_name: i.to_string(),
                        },
                        elem_type,
                        span,
                    );
                    destruct_stmts.push(TirStmt::new(
                        TirStmtKind::Let {
                            name: name.clone(),
                            local_index: local_idx,
                            is_mut,
                            is_reactive: false,
                            type_id: elem_type,
                            value: field_access,
                            skip_value_copy: false,
                        },
                        span,
                    ));
                }
            }
        }

        let index_binding =
            super::Elaborator::<H>::enumerate_index_local(is_enumerate, &for_of.binding, ctx);
        if let Some(local) = index_binding {
            ctx.variadic_enumerate_indices.push(local);
        }
        let mut body = self.reify_block(&for_of.body, ctx, None);
        if index_binding.is_some() {
            ctx.variadic_enumerate_indices.pop();
        }
        ctx.exit_scope();
        if !destruct_stmts.is_empty() {
            destruct_stmts.extend(body.stmts);
            body.stmts = destruct_stmts;
        }

        vec![TirStmt::new(
            TirStmtKind::VariadicForOf {
                iterable,
                binding_name,
                binding_local,
                is_mut,
                body,
                unique_id,
                by_ref,
                is_enumerate,
            },
            span,
        )]
    }

    /// Reify `[for let v of tuple { expr }]` into the deferred
    /// `VariadicTupleComprehension` node the monomorphizer unrolls once the
    /// pack is concrete. Mirrors [`Self::reify_variadic_for_of`]'s binding.
    fn reify_tuple_comprehension(
        &mut self,
        comp: &ast::TupleComprehensionExpr,
        ctx: &mut FunctionContext,
        recorded_type: TypeId,
    ) -> TirExpr {
        use crate::tir::{TirExprKind, TirStmtKind, TypeTable};

        let span = comp.span;
        let (source, is_enumerate) = super::Elaborator::<H>::split_enumerate(&comp.iterable);
        let iterable = self.reify_expr(source, ctx, None);
        let unique_id = ctx.next_local;

        let elem_type = super::Elaborator::<H>::comprehension_pack_elem(
            &self.tysys.type_table,
            iterable.type_id,
        )
        .unwrap_or(TypeTable::UNKNOWN);
        let binding_type = if is_enumerate {
            self.tysys
                .type_table
                .borrow_mut()
                .make_tuple(vec![TypeTable::I32, elem_type])
        } else {
            elem_type
        };

        let binding_name = match &comp.binding {
            ast::Pattern::Ident { name, .. } => name.clone(),
            _ => format!("__comp_temp_{unique_id}"),
        };
        let binding_name_span = match &comp.binding {
            ast::Pattern::Ident { span, .. } => *span,
            _ => crate::token::Span::default(),
        };
        let binding_id = match &comp.binding {
            ast::Pattern::Ident { id, .. } => Some(*id),
            _ => None,
        };

        ctx.enter_scope();
        let binding_local = ctx.add_local_at(
            binding_name.clone(),
            binding_type,
            false,
            binding_id,
            binding_name_span,
        );

        let mut destructure: Vec<TirStmt> = Vec::new();
        if let ast::Pattern::Tuple(elems, _) = &comp.binding {
            let inner = self
                .tysys
                .type_table
                .borrow()
                .as_tuple(binding_type)
                .unwrap_or_else(|| vec![binding_type]);
            for (i, elem) in elems.iter().enumerate() {
                let ast::Pattern::Ident {
                    id,
                    name,
                    span: elem_span,
                    ..
                } = elem
                else {
                    continue;
                };
                let sub_type = inner.get(i).copied().unwrap_or(TypeTable::UNKNOWN);
                let local_index =
                    ctx.add_local_at(name.clone(), sub_type, false, Some(*id), *elem_span);
                let field_access = TirExpr::new(
                    TirExprKind::FieldAccess {
                        expr: Box::new(TirExpr::new(
                            TirExprKind::Local {
                                index: binding_local,
                                name: binding_name.clone(),
                            },
                            binding_type,
                            span,
                        )),
                        field_index: i as u32,
                        field_name: i.to_string(),
                    },
                    sub_type,
                    span,
                );
                destructure.push(TirStmt::new(
                    TirStmtKind::Let {
                        name: name.clone(),
                        local_index,
                        is_mut: false,
                        is_reactive: false,
                        type_id: sub_type,
                        value: field_access,
                        skip_value_copy: false,
                    },
                    span,
                ));
            }
        }

        let index_binding =
            super::Elaborator::<H>::enumerate_index_local(is_enumerate, &comp.binding, ctx);
        if let Some(local) = index_binding {
            ctx.variadic_enumerate_indices.push(local);
        }
        let body = self.reify_expr(&comp.body, ctx, None);
        if index_binding.is_some() {
            ctx.variadic_enumerate_indices.pop();
        }
        ctx.exit_scope();

        TirExpr::new(
            TirExprKind::VariadicTupleComprehension {
                iterable: Box::new(iterable),
                binding_name,
                binding_local,
                destructure,
                body: Box::new(body),
                unique_id,
                is_enumerate,
            },
            recorded_type,
            span,
        )
    }

    /// Reify a C-style `for init; cond; update { body }` loop into
    /// the shape `Elaborator::resolve_for` produces.
    fn reify_for(&mut self, f: &ast::ForStmt, ctx: &mut FunctionContext) -> Vec<TirStmt> {
        use crate::tir::{TirBlock, TirExprKind, TirStmtKind, TirUnaryOp, TypeTable};

        let span = f.span;
        let loop_id = ctx.next_loop_id;
        ctx.next_loop_id += 1;
        let body_label = format!("$for_{loop_id}_body");

        let saved_continue = std::mem::take(&mut ctx.for_continue_labels);
        ctx.enter_scope();

        let mut outer_stmts: Vec<TirStmt> = Vec::new();
        if let Some(init) = &f.init {
            outer_stmts.extend(self.reify_stmt(init, ctx));
        }

        let iter_stmts: Vec<TirStmt> = match &f.condition {
            None => {
                let labeled_body = self.reify_for_labeled_body(&body_label, &f.body, ctx);
                let mut s = vec![labeled_body];
                s.extend(self.reify_for_update(f.update.as_ref(), ctx));
                s
            }
            Some(ast::Condition::Expr(cond_expr)) => {
                let cond_span = cond_expr.span();
                let cond_tir = self.reify_expr(cond_expr, ctx, Some(TypeTable::BOOL));
                let neg_cond = TirExpr::new(
                    TirExprKind::Unary {
                        op: TirUnaryOp::Not,
                        expr: Box::new(cond_tir),
                    },
                    TypeTable::BOOL,
                    cond_span,
                );
                let break_stmt = TirStmt::new(
                    TirStmtKind::Break {
                        label: None,
                        value: None,
                    },
                    span,
                );
                let if_break = TirStmt::new(
                    TirStmtKind::If {
                        condition: neg_cond,
                        then_block: TirBlock::new(vec![break_stmt], span),
                        else_block: None,
                    },
                    span,
                );
                let labeled_body = self.reify_for_labeled_body(&body_label, &f.body, ctx);
                let mut s = vec![if_break, labeled_body];
                s.extend(self.reify_for_update(f.update.as_ref(), ctx));
                s
            }
            Some(ast::Condition::LetChain {
                elements,
                span: cond_span,
            }) => {
                // For-let-chain is restricted to a single Let
                // element (the parser enforces this; mirror
                // `Elaborator::resolve_for`). The
                // expansion shape is a single Match: the pattern
                // arm's body is the labeled-body + update; the
                // wildcard arm breaks.
                use crate::tir::{TirExprKind, TirMatchArm, TirPattern};
                let single_let = if elements.len() == 1 {
                    match &elements[0] {
                        ast::ConditionElement::Let {
                            pattern,
                            expr,
                            span: elem_span,
                        } => Some((pattern, expr, *elem_span)),
                        _ => None,
                    }
                } else {
                    None
                };
                let Some((pattern, expr, elem_span)) = single_let else {
                    // Annotate already diagnosed multi-element
                    // for-let-chain as `InvalidPattern`; emit
                    // empty to mirror.
                    ctx.exit_scope();
                    ctx.for_continue_labels = saved_continue;
                    return vec![];
                };

                let scrutinee = self.reify_expr(expr, ctx, None);
                let scrutinee_type = scrutinee.type_id;
                ctx.enter_scope();
                let tir_pattern = self.reify_pattern(pattern, scrutinee_type, ctx);
                let labeled_body = self.reify_for_labeled_body(&body_label, &f.body, ctx);
                let update_stmts = self.reify_for_update(f.update.as_ref(), ctx);
                ctx.exit_scope();

                let mut then_stmts = vec![labeled_body];
                then_stmts.extend(update_stmts);
                let then_body = TirExpr::new(
                    TirExprKind::Block(TirBlock::new(then_stmts, *cond_span)),
                    TypeTable::UNIT,
                    *cond_span,
                );
                let else_body = TirExpr::new(
                    TirExprKind::Block(TirBlock::new(
                        vec![TirStmt::new(
                            TirStmtKind::Break {
                                label: None,
                                value: None,
                            },
                            span,
                        )],
                        *cond_span,
                    )),
                    TypeTable::NEVER,
                    *cond_span,
                );
                let arms = vec![
                    TirMatchArm {
                        pattern: tir_pattern,
                        guard: None,
                        body: then_body,
                        span: elem_span,
                    },
                    TirMatchArm {
                        pattern: TirPattern::Wildcard,
                        guard: None,
                        body: else_body,
                        span: *cond_span,
                    },
                ];
                vec![TirStmt::new(
                    TirStmtKind::Expr(TirExpr::new(
                        TirExprKind::Match {
                            expr: Box::new(scrutinee),
                            arms,
                        },
                        TypeTable::UNIT,
                        *cond_span,
                    )),
                    *cond_span,
                )]
            }
        };

        outer_stmts.push(TirStmt::new(
            TirStmtKind::Loop {
                body: TirBlock::new(iter_stmts, span),
            },
            span,
        ));

        ctx.exit_scope();
        ctx.for_continue_labels = saved_continue;
        outer_stmts
    }

    /// Reify the for-loop body wrapped in `__for_N_body:` so naked
    /// `continue` lowers as `break __for_N_body` (letting the
    /// `update` expression run before the next iteration). Mirrors
    /// `Elaborator::resolve_for_labeled_body`.
    fn reify_for_labeled_body(
        &mut self,
        body_label: &str,
        body: &ast::Block,
        ctx: &mut FunctionContext,
    ) -> TirStmt {
        use crate::tir::TirStmtKind;
        ctx.for_continue_labels.push(body_label.to_string());
        ctx.active_labels.push(body_label.to_string());
        let body_block = self.reify_block(body, ctx, None);
        ctx.active_labels.pop();
        ctx.for_continue_labels.pop();
        TirStmt::new(
            TirStmtKind::LabeledBlock {
                label: body_label.to_string(),
                block: body_block,
            },
            body.span,
        )
    }

    /// Reify the for-loop's optional `update` expression as a single
    /// stmt-list (empty when absent). Mirrors
    /// `Elaborator::resolve_for_update`.
    fn reify_for_update(
        &mut self,
        update: Option<&ast::Expr>,
        ctx: &mut FunctionContext,
    ) -> Vec<TirStmt> {
        update
            .map(|u| {
                let tir = self.reify_expr(u, ctx, None);
                vec![TirStmt::new(crate::tir::TirStmtKind::Expr(tir), u.span())]
            })
            .unwrap_or_default()
    }

    /// Reify a let-chain (`if let PAT = e [&& BOOL]* { … }`) into nested Match /
    /// If stmts, shared by `reify_if_expr` / `reify_if_stmt` / `reify_while`.
    /// A `Let` element becomes a two-arm Match (pattern arm recurses, wildcard
    /// falls back to `else_block`), an `Expr` element a single-branch `If`; the
    /// recursion bottoms out on the empty list by reifying `then_block`.
    fn reify_let_chain_stmts(
        &mut self,
        elements: &[ast::ConditionElement],
        then_block_ast: &ast::Block,
        else_block: Option<&crate::tir::TirBlock>,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
        tail_value: bool,
        span: crate::token::Span,
    ) -> Vec<TirStmt> {
        use crate::tir::{TirBlock, TirExprKind, TirMatchArm, TirPattern, TirStmtKind, TypeTable};

        if elements.is_empty() {
            return self
                .reify_block_with_position(then_block_ast, ctx, expected_type, tail_value)
                .stmts;
        }

        match &elements[0] {
            ast::ConditionElement::Let {
                pattern,
                expr,
                span: elem_span,
            } => {
                let scrutinee = self.reify_expr(expr, ctx, None);
                let scrutinee_type = scrutinee.type_id;
                let tir_pattern = self.reify_pattern(pattern, scrutinee_type, ctx);
                let inner_stmts = self.reify_let_chain_stmts(
                    &elements[1..],
                    then_block_ast,
                    else_block,
                    ctx,
                    expected_type,
                    tail_value,
                    span,
                );
                let inner_block = TirBlock::new(inner_stmts, span);
                // Use the shared `block_result_type` (mirroring
                // `resolve_let_chain_stmts`) so a then/else
                // block ending in a value `If` / `Match` / nested chain
                // contributes its real result type. A hand-rolled
                // "last stmt is Expr" check would mis-classify those
                // trailing forms as `Unit`, collapsing the Match's
                // `match_type` to `Unit` and dropping the branch values.
                let tt = self.tysys.type_table.borrow();
                let then_type = crate::tir::block_result_type(&tt, &inner_block);
                let else_tir = else_block.cloned();
                let else_type = else_tir
                    .as_ref()
                    .map_or(TypeTable::UNIT, |b| crate::tir::block_result_type(&tt, b));
                let else_arm_span = else_tir.as_ref().map_or(span, |b| b.span);
                let match_type = crate::tir::agree_branch_types(&tt, then_type, else_type)
                    .unwrap_or(TypeTable::UNIT);
                drop(tt);
                let then_body = TirExpr::new(TirExprKind::Block(inner_block), then_type, span);
                let else_body = match else_tir {
                    Some(b) => {
                        let b_span = b.span;
                        TirExpr::new(TirExprKind::Block(b), else_type, b_span)
                    }
                    None => TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, span),
                };
                let arms = vec![
                    TirMatchArm {
                        pattern: tir_pattern,
                        guard: None,
                        body: then_body,
                        span: *elem_span,
                    },
                    TirMatchArm {
                        pattern: TirPattern::Wildcard,
                        guard: None,
                        body: else_body,
                        span: else_arm_span,
                    },
                ];
                vec![TirStmt::new(
                    TirStmtKind::Expr(TirExpr::new(
                        TirExprKind::Match {
                            expr: Box::new(scrutinee),
                            arms,
                        },
                        match_type,
                        span,
                    )),
                    span,
                )]
            }
            ast::ConditionElement::Expr(expr) => {
                let condition = self.reify_expr(expr, ctx, Some(TypeTable::BOOL));
                let inner_stmts = self.reify_let_chain_stmts(
                    &elements[1..],
                    then_block_ast,
                    else_block,
                    ctx,
                    expected_type,
                    tail_value,
                    span,
                );
                let inner_block = TirBlock::new(inner_stmts, span);
                vec![TirStmt::new(
                    TirStmtKind::If {
                        condition,
                        then_block: inner_block,
                        else_block: else_block.cloned(),
                    },
                    span,
                )]
            }
        }
    }

    /// Reify a trailing stmt-position `if` whose value flows out as the
    /// enclosing block's result. Mirrors
    /// `Elaborator::resolve_if_stmt_with_expected`: the
    /// `LetChain` arm reuses the let-chain lowering with `expected_type`
    /// threaded through so the chain's then/else blocks stay
    /// value-producing; the `Expr` arm emits an `If` *expression*
    /// statement (not a value-dropping stmt `If`) so the branch values
    /// become the block result.
    fn reify_if_stmt_with_expected(
        &mut self,
        if_stmt: &ast::IfStmt,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
        tail_value: bool,
    ) -> Vec<TirStmt> {
        use crate::tir::{TirExprKind, TirStmtKind, TypeTable};
        match &if_stmt.condition {
            ast::Condition::LetChain { elements, .. } => {
                let else_block = if_stmt
                    .else_block
                    .as_ref()
                    .map(|b| self.reify_block_with_position(b, ctx, expected_type, tail_value));
                ctx.enter_scope();
                let stmts = self.reify_let_chain_stmts(
                    elements,
                    &if_stmt.then_block,
                    else_block.as_ref(),
                    ctx,
                    expected_type,
                    tail_value,
                    if_stmt.span,
                );
                ctx.exit_scope();
                stmts
            }
            ast::Condition::Expr(cond_expr) => {
                let condition = self.reify_condition_expr(cond_expr, ctx);
                let then_branch = self.reify_block_with_position(
                    &if_stmt.then_block,
                    ctx,
                    expected_type,
                    tail_value,
                );
                let else_branch = if_stmt
                    .else_block
                    .as_ref()
                    .map(|b| self.reify_block_with_position(b, ctx, expected_type, tail_value));
                let tt = self.tysys.type_table.borrow();
                let then_type = crate::tir::block_result_type(&tt, &then_branch);
                let else_type = else_branch
                    .as_ref()
                    .map_or(TypeTable::UNIT, |b| crate::tir::block_result_type(&tt, b));
                let result_type = crate::tir::agree_branch_types(&tt, then_type, else_type)
                    .unwrap_or(TypeTable::UNIT);
                drop(tt);
                let if_expr = TirExpr::new(
                    TirExprKind::If {
                        condition: Box::new(condition),
                        then_branch,
                        else_branch,
                    },
                    result_type,
                    if_stmt.span,
                );
                vec![TirStmt::new(TirStmtKind::Expr(if_expr), if_stmt.span)]
            }
        }
    }

    /// Reify a stmt-position `if cond { … } else { … }`. Stmt
    /// position never carries an `expected_type` from the surrounding
    /// block (the elaborator switches to `…_with_expected` only on a
    /// trailing position; reify follows suit by passing `None` to the
    /// branches). `Condition::LetChain` mirrors the expression-level
    /// `IfLetChain` desugar.
    fn reify_if_stmt(&mut self, if_stmt: &ast::IfStmt, ctx: &mut FunctionContext) -> Vec<TirStmt> {
        use crate::tir::TirStmtKind;
        match &if_stmt.condition {
            ast::Condition::Expr(cond_expr) => {
                let condition = self.reify_condition_expr(cond_expr, ctx);
                let then_block = self.reify_block(&if_stmt.then_block, ctx, None);
                let else_block = if_stmt
                    .else_block
                    .as_ref()
                    .map(|b| self.reify_block(b, ctx, None));
                vec![TirStmt::new(
                    TirStmtKind::If {
                        condition,
                        then_block,
                        else_block,
                    },
                    if_stmt.span,
                )]
            }
            ast::Condition::LetChain { elements, .. } => {
                // Mirror `Elaborator::resolve_if_stmt`'s
                // `Condition::LetChain` arm: the
                // chain elements lower into nested Match / If
                // stmts via the shared `reify_let_chain_stmts`.
                // Else-branch resolves in the outer scope (chain
                // bindings aren't visible there); the chain body
                // gets its own scope.
                let else_block = if_stmt
                    .else_block
                    .as_ref()
                    .map(|b| self.reify_block(b, ctx, None));
                ctx.enter_scope();
                let stmts = self.reify_let_chain_stmts(
                    elements,
                    &if_stmt.then_block,
                    else_block.as_ref(),
                    ctx,
                    None,
                    false,
                    if_stmt.span,
                );
                ctx.exit_scope();
                stmts
            }
        }
    }

    /// Reify an `if cond { … } else { … }` expression. `Condition::LetChain`
    /// dispatches through the `IfLetChain` desugar, whose tag annotate
    /// records on `sem.types.desugars` (`Elaborator::resolve_if_expr`).
    fn reify_if_expr(
        &mut self,
        if_expr: &ast::IfExpr,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
        recorded_type: TypeId,
    ) -> TirExpr {
        // With no expected type from the use site (`let x = if c { Some(v) }
        // else { null };`), fall back to the if-expression's own unified
        // `recorded_type` so the bare `null` branch reifies as `Option<T>`
        // rather than UNKNOWN — a branch type disagreeing with the `If` node's
        // own produces invalid Wasm. Same fallback as `LabeledBlockTarget`.
        let branch_expected = expected_type.or(Some(recorded_type));
        let cond_expr = match &if_expr.condition {
            ast::Condition::Expr(e) => e,
            ast::Condition::LetChain { elements, .. } => {
                // Mirror `Elaborator::resolve_if_expr`'s
                // `Condition::LetChain` arm: the
                // chain reduces to a `Block` of nested Match /
                // If stmts via `reify_let_chain_stmts`. The
                // overall block's result type is the recorded
                // `expected_type` (or `recorded_type` as a
                // fallback when no expectation propagated).
                let else_block = if_expr
                    .else_block
                    .as_ref()
                    .map(|b| self.reify_block_value(b, ctx, branch_expected));
                ctx.enter_scope();
                let stmts = self.reify_let_chain_stmts(
                    elements,
                    &if_expr.then_block,
                    else_block.as_ref(),
                    ctx,
                    branch_expected,
                    true,
                    if_expr.span,
                );
                ctx.exit_scope();
                let chain_block = crate::tir::TirBlock::new(stmts, if_expr.span);
                return TirExpr::new(
                    crate::tir::TirExprKind::Block(chain_block),
                    recorded_type,
                    if_expr.span,
                );
            }
        };
        let condition = self.reify_condition_expr(cond_expr, ctx);
        let then_branch = self.reify_block_value(&if_expr.then_block, ctx, branch_expected);
        let else_branch = if_expr
            .else_block
            .as_ref()
            .map(|b| self.reify_block_value(b, ctx, branch_expected));
        TirExpr::new(
            crate::tir::TirExprKind::If {
                condition: Box::new(condition),
                then_branch,
                else_branch,
            },
            recorded_type,
            if_expr.span,
        )
    }

    /// Reify a binary expression. When the elaborator dispatched the
    /// operator to a trait method, the
    /// `sem.types.operator_dispatch[binary.id]` entry carries the
    /// `(FunctionRef, self_kind, arg_ref_wraps, return_type)` reify
    /// needs to emit the same method-call shape. Absence
    /// of an entry means the elaborator emitted a native
    /// `TirExprKind::Binary`; reify mirrors with the 1:1 op mapping.
    fn reify_binary(
        &mut self,
        binary: &ast::BinaryExpr,
        ctx: &mut FunctionContext,
        recorded_type: TypeId,
    ) -> TirExpr {
        use crate::tir::{CallArg, ResolvedType, TirBinaryOp, TirExprKind, TirUnaryOp, TypeTable};

        // Mirror `resolve_binary_operands_with_coercion`:
        // a numeric-literal operand is typed from the *other* operand (or,
        // when both are literals, from the expression's recorded type). This
        // matters for inlined associated-const bodies like
        // `f32::INFINITY = 1.0 / 0.0`, whose literals carry no recorded type
        // of their own — without the hint they default to `f64` and the
        // surrounding arithmetic lowers to the wrong width / an integer op.
        let left_is_lit = self.tysys.is_numeric_literal(&binary.left);
        let right_is_lit = self.tysys.is_numeric_literal(&binary.right);
        let (left, right) = if left_is_lit && !right_is_lit {
            let right = self.reify_expr(&binary.right, ctx, None);
            let coerce = if self.tysys.type_table.borrow().is_numeric(right.type_id) {
                Some(right.type_id)
            } else {
                None
            };
            let left = self.reify_expr(&binary.left, ctx, coerce);
            (left, right)
        } else if right_is_lit && !left_is_lit {
            let left = self.reify_expr(&binary.left, ctx, None);
            let coerce = if self.tysys.type_table.borrow().is_numeric(left.type_id) {
                Some(left.type_id)
            } else {
                None
            };
            let right = self.reify_expr(&binary.right, ctx, coerce);
            (left, right)
        } else if left_is_lit && right_is_lit {
            // Both literals: use the expression's recorded type as the hint
            // (e.g. the `const_ty` flowing in from a reified const body).
            let hint = if recorded_type == TypeTable::UNKNOWN {
                None
            } else {
                Some(recorded_type)
            };
            let left = self.reify_expr(&binary.left, ctx, hint);
            let right = self.reify_expr(&binary.right, ctx, hint);
            (left, right)
        } else if matches!(
            binary.op,
            ast::BinaryOp::Eq
                | ast::BinaryOp::NotEq
                | ast::BinaryOp::Lt
                | ast::BinaryOp::LtEq
                | ast::BinaryOp::Gt
                | ast::BinaryOp::GtEq
        ) && super::Elaborator::<H>::takes_shape_from_expected_type(&binary.left)
            && !super::Elaborator::<H>::takes_shape_from_expected_type(&binary.right)
        {
            let right = self.reify_expr(&binary.right, ctx, None);
            let left = self.reify_expr(&binary.left, ctx, None);
            (left, right)
        } else {
            let left = self.reify_expr(&binary.left, ctx, None);
            let right = self.reify_expr(&binary.right, ctx, None);
            (left, right)
        };

        // Reference equality: when both operands are references, the
        // elaborator emits `RefEq` / `RefNotEq` (identity comparison)
        // rather than dispatching to `Eq` — and records no operator
        // dispatch. The decision is from operand types alone, so reify
        // reproduces it here.
        if matches!(binary.op, ast::BinaryOp::Eq | ast::BinaryOp::NotEq) {
            let both_refs = {
                let tt = self.tysys.type_table.borrow();
                matches!(
                    (tt.get(left.type_id), tt.get(right.type_id)),
                    (ResolvedType::Ref(_), ResolvedType::Ref(_))
                        | (ResolvedType::MutRef(_), ResolvedType::MutRef(_))
                )
            };
            if both_refs {
                let op = if binary.op == ast::BinaryOp::Eq {
                    TirBinaryOp::RefEq
                } else {
                    TirBinaryOp::RefNotEq
                };
                return TirExpr::new(
                    crate::tir::TirExprKind::Binary {
                        op,
                        left: Box::new(left),
                        right: Box::new(right),
                    },
                    TypeTable::BOOL,
                    binary.span,
                );
            }
        }

        if let Some(dispatch) = self.ann_operator_dispatch(binary.id) {
            // Operator-trait dispatch path. Reuse the shared receiver
            // adjuster (statically; no Elaborator needed) and the
            // shared arg-wrap helper to produce TIR identical to what
            // `build_trait_op_method_call_on_resolved` emitted.
            let receiver = adjust_receiver_for_self_kind(
                left,
                dispatch.self_kind,
                /* is_ref_impl */ false,
                binary.span,
                &self.tysys.type_table,
            );
            let args = vec![right];
            let call_args: Vec<CallArg> = args
                .into_iter()
                .zip(dispatch.arg_ref_wraps.iter().copied())
                .map(|(arg, wrap)| {
                    let arg_expr = if wrap {
                        let arg_ref_type = self
                            .tysys
                            .type_table
                            .borrow_mut()
                            .intern(ResolvedType::Ref(arg.type_id));
                        TirExpr::new(
                            TirExprKind::Unary {
                                op: TirUnaryOp::Ref,
                                expr: Box::new(arg),
                            },
                            arg_ref_type,
                            binary.span,
                        )
                    } else {
                        arg
                    };
                    CallArg::new(arg_expr, false)
                })
                .collect();
            let call = build_tir_method_call(
                receiver,
                dispatch.function_ref,
                vec![],
                call_args,
                dispatch.return_type,
                binary.span,
            );
            // Comparison operators dispatch to `Eq::eq` / `Ord::cmp` but
            // the source operator decides the wrapping the elaborator
            // applies after the call: `!=` negates the `eq` result, and
            // `<` / `>` / `<=` / `>=` compare the `cmp` `Ordering` against
            // the variant that makes the operator true.
            return match binary.op {
                ast::BinaryOp::NotEq if call.type_id == TypeTable::BOOL => TirExpr::new(
                    TirExprKind::Unary {
                        op: TirUnaryOp::Not,
                        expr: Box::new(call),
                    },
                    TypeTable::BOOL,
                    binary.span,
                ),
                ast::BinaryOp::Lt
                | ast::BinaryOp::Gt
                | ast::BinaryOp::LtEq
                | ast::BinaryOp::GtEq
                    if call.type_id != TypeTable::ERROR =>
                {
                    ord_bool_from_cmp(call, binary.op, binary.span, &self.tysys.type_table)
                }
                _ => call,
            };
        }

        // Native binary op — primitive path. The op mapping is 1:1 with the
        // AST. Ref-equality (`RefEq` / `RefNotEq`) is synthesised by the
        // elaborator after type analysis; until that decision is recorded,
        // reify emits the source-level op verbatim. That affects only the
        // `==` / `!=` path on ref types; other ops on refs are already
        // diagnosed by annotate.
        TirExpr::new(
            crate::tir::TirExprKind::Binary {
                left: Box::new(left),
                op: ast_binary_op_to_tir(binary.op),
                right: Box::new(right),
            },
            recorded_type,
            binary.span,
        )
    }

    /// Reify a template string `` `…${expr}…` ``: no interpolations concatenates
    /// to a `StringLiteral` at reify time, a lone `String` interpolation with no
    /// format spec forwards its expression unchanged, and everything else builds
    /// `Vec<TirTemplatePart>` with specs from [`crate::format_spec::parse`].
    fn reify_template_string(
        &mut self,
        template: &ast::TemplateStringExpr,
        ctx: &mut FunctionContext,
        _recorded_type: TypeId,
    ) -> TirExpr {
        use crate::tir::{TirExprKind, TirTemplatePart};

        let string_type = self
            .tysys
            .type_table
            .borrow_mut()
            .make_compiler_struct(crate::compiler_item::CompilerItem::String);
        let span = template.span;

        let has_interpolation = template
            .parts
            .iter()
            .any(|p| matches!(p, ast::TemplatePart::Interpolation { .. }));

        if !has_interpolation {
            let mut combined = String::new();
            for part in &template.parts {
                if let ast::TemplatePart::String(s) = part {
                    combined.push_str(&super::util::unescape_template_segment(s));
                }
            }
            return TirExpr::new(TirExprKind::StringLiteral(combined), string_type, span);
        }

        if template.parts.len() == 1
            && let ast::TemplatePart::Interpolation { expr, format: None } = &template.parts[0]
        {
            let resolved = self.reify_expr(expr, ctx, None);
            if resolved.type_id == string_type {
                return resolved;
            }
        }

        let mut parts = Vec::new();
        for part in &template.parts {
            match part {
                ast::TemplatePart::String(s) => {
                    let unescaped = super::util::unescape_template_segment(s);
                    if !unescaped.is_empty() {
                        parts.push(TirTemplatePart::Literal(unescaped));
                    }
                }
                ast::TemplatePart::Interpolation { expr, format } => {
                    let resolved = self.reify_expr(expr, ctx, None);
                    let format_spec = format.as_ref().map(|f| {
                        crate::format_spec::parse(&f.spec)
                            .expect("the parser rejects a malformed format specifier")
                    });
                    parts.push(TirTemplatePart::Interpolation {
                        expr: Box::new(resolved),
                        format_spec,
                    });
                }
            }
        }

        TirExpr::new(TirExprKind::TemplateString { parts }, string_type, span)
    }

    /// Reify a tagged template as the call annotate resolved it: the tag on a
    /// literal of the template's anonymous type, one field per hole. A hole
    /// that is a place is read or borrowed where it stands; any other is
    /// evaluated once, in source order, into a local of the block the call
    /// then sits in (WEP 2026-01-10).
    fn reify_tagged_template(
        &mut self,
        tagged: &ast::TaggedTemplateExpr,
        ctx: &mut FunctionContext,
        recorded_type: TypeId,
    ) -> TirExpr {
        use crate::tir::{
            CallArg, TemplateShape, TirBlock, TirExprKind, TirStmt, TirStmtKind, TirStructField,
        };

        let span = tagged.span;
        let (Some(template_ty), Some(dispatch)) = (
            self.ann_tagged_template(tagged.id),
            self.ann_static_method_dispatch(tagged.id),
        ) else {
            // Annotate diagnosed the template or the tag.
            return TirExpr::new(TirExprKind::Unit, TypeTable::ERROR, span);
        };
        let (struct_name, hole_types) = {
            let tt = self.tysys.type_table.borrow();
            let crate::tir::ResolvedType::Struct { def, .. } = tt.get(template_ty) else {
                panic!("annotate records a struct type for a tagged template");
            };
            let shape = tt
                .template_shape_of_type(template_ty)
                .expect("annotate records a template type for a tagged template");
            let holes: Vec<TypeId> = shape.holes.iter().map(|h| h.ty).collect();
            // The name the shape was registered under, not the one a message
            // shows: both render the shape, and only the mangle is a key.
            (tt.struct_head_name(*def), holes)
        };

        let unique_id = ctx.next_local;
        let mut stmts: Vec<TirStmt> = Vec::new();
        let mut fields: Vec<TirStructField> = Vec::new();
        let holes: Vec<&ast::Expr> = tagged.template.interpolations().collect();
        let values: Vec<TirExpr> = holes
            .iter()
            .map(|expr| self.reify_expr(expr, ctx, None))
            .collect();
        // A place is read where it stands only while nothing between there and
        // the call can write it. The struct literal is built after every
        // hoisted hole has run, so a place followed by a hole that is not one
        // is read too late and must be bound at its own position instead.
        let last_hoisted = values.iter().rposition(|v| !Self::is_place_hole(v));
        for (k, ((expr, mut value), &hole_ty)) in
            holes.iter().zip(values).zip(&hole_types).enumerate()
        {
            if last_hoisted.is_some_and(|last| k <= last) {
                let name = format!("__hole_{unique_id}_{k}");
                let local_index = ctx.add_local(name.clone(), hole_ty, false, None);
                stmts.push(TirStmt::new(
                    TirStmtKind::Let {
                        name: name.clone(),
                        local_index,
                        is_mut: false,
                        is_reactive: false,
                        type_id: hole_ty,
                        value,
                        skip_value_copy: false,
                    },
                    expr.span(),
                ));
                value = TirExpr::new(
                    TirExprKind::Local {
                        index: local_index,
                        name,
                    },
                    hole_ty,
                    expr.span(),
                );
            }
            let field_ty = self.tysys.type_table.borrow_mut().hole_field_type(hole_ty);
            if field_ty != hole_ty {
                if let TirExprKind::Local { index, .. } = &value.kind {
                    ctx.address_taken_locals.insert(*index);
                }
                value = TirExpr::new(
                    TirExprKind::Unary {
                        op: crate::tir::TirUnaryOp::Ref,
                        expr: Box::new(value),
                    },
                    field_ty,
                    expr.span(),
                );
            }
            fields.push(TirStructField {
                name: TemplateShape::field_name(k),
                value,
                field_index: k as u32,
            });
        }

        let literal = TirExpr::new(
            TirExprKind::StructLiteral {
                struct_type: template_ty,
                struct_name,
                fields,
            },
            template_ty,
            span,
        );
        // The template is the tag's one written argument; a trailing parameter
        // with a default is filled here, as it is for a spelled call.
        let mut args = vec![CallArg::new(literal, false)];
        self.reify_pad_dispatch_defaults(&tagged.tag, &mut args, &dispatch, span, ctx);
        let call = TirExpr::new(
            TirExprKind::Call {
                type_args: dispatch.type_args,
                func: Box::new(dispatch.function_ref),
                args,
                has_receiver: false,
            },
            recorded_type,
            span,
        );
        if stmts.is_empty() {
            return call;
        }

        let label = format!("$tagged_{unique_id}");
        stmts.push(TirStmt::new(
            TirStmtKind::Break {
                label: Some(label.clone()),
                value: Some(call),
            },
            span,
        ));
        TirExpr::new(
            TirExprKind::LabeledBlock {
                label,
                block: TirBlock::new(stmts, span),
                result_type: recorded_type,
            },
            recorded_type,
            span,
        )
    }

    /// Whether a reified hole names storage the template literal can read or
    /// borrow in situ: a local, a parameter, a global, or a field or element
    /// chain rooted at one. Anything else computes a value, which the literal
    /// reads once from a temporary instead.
    ///
    /// Decided on the reified value rather than the hole's spelling, so it
    /// answers by what the hole *is* — resolution already settled that — and
    /// not by a name looked up a second time.
    fn is_place_hole(value: &TirExpr) -> bool {
        match &value.kind {
            TirExprKind::Local { .. } | TirExprKind::GlobalVarGet { .. } => true,
            TirExprKind::FieldAccess { expr, .. } | TirExprKind::Index { expr, .. } => {
                Self::is_place_hole(expr)
            }
            _ => false,
        }
    }

    /// Reify a `a..<b` / `a..=b` range expression. The elaborator
    /// lowers ranges into the prelude's `RangeExclusive` /
    /// `RangeInclusive` struct literals; reify
    /// produces the same shape by reading the element type from
    /// the reified `start` expression and interning the
    /// `GenericInstance` via `make_generic_instance`.
    fn reify_range(
        &mut self,
        range: &ast::RangeExpr,
        ctx: &mut FunctionContext,
        recorded_type: TypeId,
    ) -> TirExpr {
        use crate::ast::RangeKind;
        use crate::tir::{TirExprKind, TirStructField, TypeTable};

        // Resolve both operands first; the element type comes from
        // `start` (annotate has unified start/end to the same type, so
        // either operand's type works).
        let start = self.reify_expr(&range.start, ctx, None);
        let end_expected = Some(start.type_id);
        let end = self.reify_expr(&range.end, ctx, end_expected);
        let element_type = start.type_id;

        // The recorded `expression_types[range.id]` carries the
        // assembled `GenericInstance` type, but the elaborator's
        // construction is purely from the prelude's compiler-item
        // registry — reproduce here so the same `module_source` lands
        // even if a future inference change made the recorded type
        // less specific.
        let item = match range.kind {
            RangeKind::Exclusive => crate::compiler_item::CompilerItem::RangeExclusive,
            RangeKind::Inclusive => crate::compiler_item::CompilerItem::RangeInclusive,
        };
        let struct_name = self
            .tysys
            .type_table
            .borrow()
            .compiler_items()
            .struct_name(item)
            .to_string();

        let struct_type = {
            let def = self
                .tysys
                .type_table
                .borrow()
                .require_compiler_item_def(item);
            self.tysys
                .type_table
                .borrow_mut()
                .make_generic_instance(def, vec![element_type])
        };

        let mut fields = vec![
            TirStructField {
                name: "start".to_string(),
                value: start,
                field_index: 0,
            },
            TirStructField {
                name: "end".to_string(),
                value: end,
                field_index: 1,
            },
        ];
        if matches!(range.kind, RangeKind::Inclusive) {
            fields.push(TirStructField {
                name: "exhausted".to_string(),
                value: TirExpr::new(TirExprKind::BoolLiteral(false), TypeTable::BOOL, range.span),
                field_index: 2,
            });
        }

        // Single source of truth: `resolve_range` recorded the mangled
        // struct name (`RangeExclusive<i32>` etc.) on the same
        // `GenericInstantiation` slot reify already consults for struct
        // literals. Falls back to the bare name on the off chance the
        // recording is absent (e.g. a recovery path).
        let mangled_name = self
            .ann_generic_instantiations(range.id)
            .and_then(|gi| gi.mangled_name)
            .unwrap_or_else(|| struct_name.clone());

        // Honour the recorded result type if present (annotate may
        // have unified with a more specific `RangeInclusive<i32>` etc.
        // already on `recorded_type`); reify trusts it as the final
        // expression type.
        let _ = recorded_type;

        TirExpr::new(
            TirExprKind::StructLiteral {
                struct_type,
                struct_name: mangled_name,
                fields,
            },
            struct_type,
            range.span,
        )
    }

    /// Reify a named `StructLiteralExpr`. Field types come from
    /// `tysys.all_struct_fields`; the instance type + `type_args` for
    /// generic structs come from the
    /// `sem.types.generic_instantiations[id]` record. Anonymous
    /// struct literals (`{ x: 1, y: 2 }` with no leading type name)
    /// flow through a different elaborator helper.
    fn reify_struct_literal(
        &mut self,
        struct_lit: &ast::StructLiteralExpr,
        ctx: &mut FunctionContext,
        recorded_type: TypeId,
    ) -> TirExpr {
        use crate::tir::{TirExprKind, TirStructField};

        // A recorded `key_value_coercions[struct_lit.id]` means the literal
        // builds an `Array<[K, V]>` for the target's `From`.
        if let Some(facts) = self.ann_key_value_coercions(struct_lit.id) {
            return self.reify_key_value_coercion(struct_lit, facts, ctx, struct_lit.span);
        }

        // Resolve the decl's canonical (name, module) from `recorded_type` —
        // the struct type annotate already resolved — instead of re-resolving
        // the source-written name, which may be an import alias or `ns::Type`.
        // A second by-name lookup could pick a same-named struct from another
        // module (issue #1416); `recorded_type` carries the definer directly.
        let struct_head =
            super::expr::peel_to_struct(&self.tysys.type_table.borrow(), recorded_type)
                .map(|(head, _)| head);

        // What the literal *is*, which annotate recorded on the type — not
        // how it was spelled. Dispatching on the spelling made reify disagree
        // with the pass that decided.
        if struct_lit.name.is_none() && !matches!(struct_head, Some(crate::tir::StructDef::Decl(_)))
        {
            return self.reify_anonymous_struct_literal(struct_lit, ctx, recorded_type);
        }
        // The storage name the WIR struct registry is keyed by.
        let struct_name = struct_lit.name.clone().unwrap_or_else(|| {
            self.tysys
                .type_table
                .borrow()
                .nominal_head(recorded_type)
                .map(|(n, _)| n)
                .unwrap_or_default()
        });
        let struct_module = match struct_head {
            Some(crate::tir::StructDef::Decl(def)) => {
                self.tysys.resolutions.defs().module(def).clone()
            }
            _ => self.current_module_source.clone(),
        };
        // Decl field shape: (name, index, raw_type, default_expr), cloned out
        // of the lookup so the borrow ends before reifying.
        let lookup = self.type_lookup();
        let info = struct_head.and_then(|head| lookup.struct_fields_of_head(head));
        let decl_fields: Vec<(String, u32, TypeId, Option<ast::Expr>)> = {
            info.map(|info| {
                info.fields
                    .iter()
                    .enumerate()
                    .map(|(i, (n, t, _vis))| {
                        let default = info.field_defaults.get(i).and_then(Option::clone);
                        (n.clone(), i as u32, *t, default)
                    })
                    .collect()
            })
            .unwrap_or_default()
        };
        let field_names_to_index: crate::hashmap::IndexMap<String, (u32, TypeId)> = decl_fields
            .iter()
            .map(|(n, i, t, _)| (n.clone(), (*i, *t)))
            .collect();

        // Only generic structs get a `generic_instantiations` record; for a
        // non-generic one, use the bare struct type from `recorded_type` plus
        // the source-level struct name as-is.
        let gi = self.ann_generic_instantiations(struct_lit.id);
        let (struct_type, generic_args): (TypeId, Vec<TypeId>) = gi
            .as_ref()
            .map(|gi| (gi.instance_type, gi.type_args.clone()))
            .unwrap_or((recorded_type, Vec::new()));
        let mangled_struct_name = gi.and_then(|gi| gi.mangled_name).unwrap_or(struct_name);

        // Substitute the decl's `TypeParam`s with the instance's generic
        // args so a field's expected type is concrete (a no-op for
        // non-generic structs, where `generic_args` is empty).
        let substitute = |this: &Self, raw: TypeId| -> TypeId {
            if generic_args.is_empty() {
                return raw;
            }
            let subst: crate::hashmap::IndexMap<u32, TypeId> = (0..generic_args.len() as u32)
                .zip(generic_args.iter().copied())
                .collect();
            this.tysys
                .type_table
                .borrow_mut()
                .substitute_type_params(raw, &subst)
        };

        // Reify each AST-provided field, then synthesize omitted fields
        // that declared a default (`port: i32 = 8080`). Field order in
        // the TIR is by declaration index — matching
        // `Elaborator::resolve_struct_literal`, which sorts after
        // filling defaults so codegen's positional slots line up.
        let mut fields: Vec<TirStructField> = struct_lit
            .fields
            .iter()
            .map(|f| {
                let (field_index, raw_ty) = field_names_to_index
                    .get(&f.name)
                    .copied()
                    .unwrap_or((0, crate::tir::TypeTable::UNKNOWN));
                let expected_field_ty = substitute(self, raw_ty);
                let value = self.reify_expr(&f.value, ctx, Some(expected_field_ty));
                TirStructField {
                    name: f.name.clone(),
                    value,
                    field_index,
                }
            })
            .collect();

        let provided: crate::hashmap::IndexSet<String> =
            struct_lit.fields.iter().map(|f| f.name.clone()).collect();

        // Fill omitted fields from `base.field` (not defaults), evaluating a
        // non-trivial `base` once via a `__base_N` temporary.
        let mut spread_binding: Option<(u32, String, TirExpr)> = None;
        if let Some(spread) = struct_lit.spreads.first() {
            let base_expr = self.reify_expr(&spread.expr, ctx, Some(struct_type));
            let base_type = base_expr.type_id;
            let base_ref = if matches!(base_expr.kind, TirExprKind::Local { .. }) {
                base_expr
            } else {
                let tmp_name = format!("__base_{}", ctx.next_local);
                let tmp_idx = ctx.add_local(tmp_name.clone(), base_type, false, None);
                spread_binding = Some((tmp_idx, tmp_name.clone(), base_expr));
                TirExpr::new(
                    TirExprKind::Local {
                        index: tmp_idx,
                        name: tmp_name,
                    },
                    base_type,
                    struct_lit.span,
                )
            };
            for (name, field_index, raw_ty, _default) in &decl_fields {
                if provided.contains(name) {
                    continue;
                }
                let field_ty = substitute(self, *raw_ty);
                fields.push(TirStructField {
                    name: name.clone(),
                    value: TirExpr::new(
                        TirExprKind::FieldAccess {
                            expr: Box::new(base_ref.clone()),
                            field_index: *field_index,
                            field_name: name.clone(),
                        },
                        field_ty,
                        struct_lit.span,
                    ),
                    field_index: *field_index,
                });
            }
        } else {
            for (name, field_index, raw_ty, default) in &decl_fields {
                if provided.contains(name) {
                    continue;
                }
                if let Some(default_expr) = default {
                    let expected_field_ty = substitute(self, *raw_ty);
                    // Reify a foreign default under its owning module's
                    // perspective: fact lookups key by the node's own globally-
                    // unique `AstId` (no module qualifier), but the default's
                    // free identifiers and decl lookups still resolve in the
                    // struct module's scope, so the perspective swap remains for
                    // name resolution.
                    let value = if struct_module == self.current_module_source {
                        self.reify_expr(default_expr, ctx, Some(expected_field_ty))
                    } else {
                        self.with_const_module_perspective(&struct_module, |this| {
                            this.reify_expr(default_expr, ctx, Some(expected_field_ty))
                        })
                    };
                    fields.push(TirStructField {
                        name: name.clone(),
                        value,
                        field_index: *field_index,
                    });
                }
            }
        }
        fields.sort_by_key(|f| f.field_index);

        let literal = TirExpr::new(
            TirExprKind::StructLiteral {
                struct_type,
                struct_name: mangled_struct_name,
                fields,
            },
            struct_type,
            struct_lit.span,
        );

        match spread_binding {
            None => literal,
            Some((idx, name, value)) => {
                use crate::tir::{TirBlock, TirStmt, TirStmtKind};
                let type_id = value.type_id;
                let stmts = vec![
                    TirStmt::new(
                        TirStmtKind::Let {
                            name,
                            local_index: idx,
                            value,
                            is_mut: false,
                            is_reactive: false,
                            type_id,
                            skip_value_copy: false,
                        },
                        struct_lit.span,
                    ),
                    TirStmt::new(TirStmtKind::Expr(literal), struct_lit.span),
                ];
                TirExpr::new(
                    TirExprKind::Block(TirBlock::new(stmts, struct_lit.span)),
                    struct_type,
                    struct_lit.span,
                )
            }
        }
    }

    /// Bind each collected impure sub-piece to a `let __caN = …;` in `prelude`
    /// once and register its `AstId` → `Local` override, so every recurrence
    /// in the reified read / write becomes that `Local` read.
    fn bind_compound_hoists(
        &mut self,
        hoists: &[CompoundHoist<'_>],
        prelude: &mut Vec<TirStmt>,
        ctx: &mut FunctionContext,
    ) {
        for (counter, hoist) in hoists.iter().enumerate() {
            let value = self.reify_expr(hoist.piece, ctx, None);
            let span = value.span;
            let type_id = value.type_id;
            let name = format!("__ca{counter}");
            let index = ctx.add_local(name.clone(), type_id, false, None);
            prelude.push(TirStmt::new(
                TirStmtKind::Let {
                    name: name.clone(),
                    local_index: index,
                    is_mut: false,
                    is_reactive: false,
                    type_id,
                    value,
                    skip_value_copy: false,
                },
                span,
            ));
            let local = TirExpr::new(TirExprKind::Local { index, name }, type_id, span);
            self.compound_overrides.insert(hoist.piece.id(), local);
        }
    }

    /// Wrap the compound-assign write in a `Block` that runs `prelude`
    /// (the once-bound sub-piece `let`s) first. With no bindings the flat
    /// write is returned unchanged, so pure targets (`x += 1`, `g += 1`)
    /// keep their previous WIR shape.
    fn wrap_prelude(
        &self,
        prelude: Vec<TirStmt>,
        write: TirExpr,
        span: crate::token::Span,
    ) -> TirExpr {
        if prelude.is_empty() {
            return write;
        }
        let mut stmts = prelude;
        stmts.push(TirStmt::new(TirStmtKind::Expr(write), span));
        TirExpr::new(
            TirExprKind::Block(TirBlock::new(stmts, span)),
            TypeTable::UNIT,
            span,
        )
    }

    /// Build the combined value `read OP rhs`, dispatching through the
    /// operator trait method when annotate recorded one on the compound's
    /// `AstId` (`u128 /= u128` → `Div::div`); a raw primitive `Binary` on
    /// struct operands would lower to invalid Wasm. Mirrors the
    /// `reify_binary` dispatch path (keyed on `binary.id`).
    fn build_compound_combined(
        &mut self,
        read: TirExpr,
        rhs: TirExpr,
        op: TirBinaryOp,
        compound: &ast::CompoundAssignExpr,
    ) -> TirExpr {
        let combined_type = read.type_id;
        if let Some(dispatch) = self.ann_operator_dispatch(compound.id) {
            let receiver = adjust_receiver_for_self_kind(
                read,
                dispatch.self_kind,
                /* is_ref_impl */ false,
                compound.span,
                &self.tysys.type_table,
            );
            let call_args: Vec<CallArg> = std::iter::once(rhs)
                .zip(dispatch.arg_ref_wraps.iter().copied())
                .map(|(arg, wrap)| {
                    let arg_expr = if wrap {
                        let arg_ref_type = self
                            .tysys
                            .type_table
                            .borrow_mut()
                            .intern(ResolvedType::Ref(arg.type_id));
                        TirExpr::new(
                            TirExprKind::Unary {
                                op: TirUnaryOp::Ref,
                                expr: Box::new(arg),
                            },
                            arg_ref_type,
                            compound.span,
                        )
                    } else {
                        arg
                    };
                    CallArg::new(arg_expr, false)
                })
                .collect();
            build_tir_method_call(
                receiver,
                dispatch.function_ref,
                vec![],
                call_args,
                dispatch.return_type,
                compound.span,
            )
        } else {
            TirExpr::new(
                crate::tir::TirExprKind::Binary {
                    left: Box::new(read),
                    op,
                    right: Box::new(rhs),
                },
                combined_type,
                compound.span,
            )
        }
    }

    /// The type of the `*…` result a dispatched index read produces. Each site
    /// supplies the type it recorded; one that is missing or unresolved is
    /// re-derived by peeling the `&Output` the reference index traits return.
    fn index_deref_type(
        &self,
        recorded: Option<TypeId>,
        dispatch: &super::sem::types::OperatorDispatch,
    ) -> TypeId {
        if !dispatch.needs_deref {
            return dispatch.return_type;
        }
        recorded
            .filter(|t| {
                !matches!(
                    self.tysys.type_table.borrow().get(*t),
                    ResolvedType::Unknown
                )
            })
            .unwrap_or_else(
                || match self.tysys.type_table.borrow().get(dispatch.return_type) {
                    ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => *inner,
                    _ => dispatch.return_type,
                },
            )
    }

    /// Build the `Index` / `IndexValue` trait read `*recv.index(idx)` (or
    /// `recv.index_value(idx)`) from an already-reified receiver and subscript
    /// plus the recorded dispatch. Shared by [`Self::reify_index`] and the
    /// compound-assign read so the two lowerings cannot drift. `deref_type` is
    /// the type of the `*…` result used when `dispatch.needs_deref`.
    fn build_index_read_from_dispatch(
        &self,
        receiver: TirExpr,
        idx: TirExpr,
        dispatch: super::sem::types::OperatorDispatch,
        deref_type: TypeId,
        span: crate::token::Span,
    ) -> TirExpr {
        let adjusted = adjust_receiver_for_self_kind(
            receiver,
            dispatch.self_kind,
            false,
            span,
            &self.tysys.type_table,
        );
        let method_call = build_tir_method_call(
            adjusted,
            dispatch.function_ref,
            vec![],
            vec![CallArg::new(idx, false)],
            dispatch.return_type,
            span,
        );
        // `Index` returns `&Output` (wrap in `*`); `IndexValue` returns
        // `Output` by copy.
        if dispatch.needs_deref {
            TirExpr::new(
                TirExprKind::Unary {
                    op: TirUnaryOp::Deref,
                    expr: Box::new(method_call),
                },
                deref_type,
                span,
            )
        } else {
            method_call
        }
    }

    /// The compound-assign read side of `recv[idx]`, reusing the same
    /// once-evaluated receiver / subscript the write side gets.
    fn build_index_trait_read(
        &self,
        recv: &TirExpr,
        idx: &TirExpr,
        index_expr: &ast::IndexExpr,
        span: crate::token::Span,
    ) -> TirExpr {
        let Some(dispatch) = self.ann_operator_dispatch(index_expr.id) else {
            // Write-only `IndexAssign` type — annotate diagnosed the missing
            // read; match its recovery shape.
            return TirExpr::new(TirExprKind::Unit, TypeTable::ERROR, span);
        };
        // Deref result type: the index expr's recorded type, peeling
        // `&Output` on the degenerate missing-annotation path.
        let deref_type = self.index_deref_type(self.ann_expression_types(index_expr.id), &dispatch);
        self.build_index_read_from_dispatch(recv.clone(), idx.clone(), dispatch, deref_type, span)
    }

    /// Reify a compound assignment `x += y` as `x = x op y`, evaluating each
    /// target sub-expression once: impure value operands bind to `let __caN`
    /// while the place skeleton stays inline. Residual: a nested index's
    /// receiver (`m.index(i)` in `m[i][j] += 1`) is still duplicated, so a
    /// side-effecting custom `Index::index` runs twice.
    fn reify_compound_assign(
        &mut self,
        compound: &ast::CompoundAssignExpr,
        ctx: &mut FunctionContext,
        recorded_type: TypeId,
    ) -> TirExpr {
        let _ = recorded_type;

        let op = match compound.op {
            CompoundAssignOp::Add => TirBinaryOp::Add,
            CompoundAssignOp::Sub => TirBinaryOp::Sub,
            CompoundAssignOp::Mul => TirBinaryOp::Mul,
            CompoundAssignOp::Div => TirBinaryOp::Div,
            CompoundAssignOp::Mod => TirBinaryOp::Mod,
            CompoundAssignOp::BitAnd => TirBinaryOp::BitAnd,
            CompoundAssignOp::BitOr => TirBinaryOp::BitOr,
            CompoundAssignOp::BitXor => TirBinaryOp::BitXor,
            CompoundAssignOp::Shl => TirBinaryOp::Shl,
            CompoundAssignOp::Shr => TirBinaryOp::Shr,
        };
        let span = compound.span;

        // Save/restore the override map so a nested compound assign keeps its
        // own bindings; the scope keeps `__caN` names out of the enclosing map.
        let mut hoists: Vec<CompoundHoist<'_>> = Vec::new();
        collect_compound_hoists(&compound.target, &mut hoists);
        let saved_overrides = std::mem::take(&mut self.compound_overrides);
        ctx.enter_scope();
        let mut prelude: Vec<TirStmt> = Vec::new();
        self.bind_compound_hoists(&hoists, &mut prelude, ctx);

        let result = self.reify_compound_assign_body(compound, ctx, op, prelude, span);
        ctx.exit_scope();
        self.compound_overrides = saved_overrides;
        result
    }

    /// Build the read + write of a compound assign with the impure-operand
    /// overrides active. Split out so `reify_compound_assign` restores the
    /// override map on every return path.
    fn reify_compound_assign_body(
        &mut self,
        compound: &ast::CompoundAssignExpr,
        ctx: &mut FunctionContext,
        op: TirBinaryOp,
        prelude: Vec<TirStmt>,
        span: crate::token::Span,
    ) -> TirExpr {
        // `recv[idx] OP= v` on an `IndexAssign` type: the read
        // (`*recv.index(idx)`) and the write (`recv.index_assign(idx, …)`) are
        // different methods built from the same reified receiver / subscript.
        if let ast::Expr::Index(index_expr) = &compound.target
            && let Some(assign_dispatch) = self.ann_index_assign_dispatch(index_expr.id)
        {
            let recv = self.reify_expr(&index_expr.expr, ctx, None);
            let idx = self.reify_expr(&index_expr.index, ctx, None);

            let read = self.build_index_trait_read(&recv, &idx, index_expr, span);
            let rhs = self.reify_expr(&compound.value, ctx, Some(read.type_id));
            let combined = self.build_compound_combined(read, rhs, op, compound);

            let write_recv = adjust_receiver_for_self_kind(
                recv,
                assign_dispatch.self_kind,
                false,
                span,
                &self.tysys.type_table,
            );
            let write = build_tir_method_call(
                write_recv,
                assign_dispatch.function_ref,
                vec![],
                vec![CallArg::new(idx, false), CallArg::new(combined, false)],
                assign_dispatch.return_type,
                span,
            );
            return self.wrap_prelude(prelude, write, span);
        }

        // General l-values (local, global, field-access, tuple index, deref):
        // the reified place's impure operands are already overridden to bound
        // locals, so read (a clone) and write share a pure skeleton.
        let place = self.reify_expr(&compound.target, ctx, None);
        let rhs = self.reify_expr(&compound.value, ctx, Some(place.type_id));
        let combined = self.build_compound_combined(place.clone(), rhs, op, compound);

        let write = if let TirExprKind::GlobalVarGet {
            module_source,
            name,
        } = &place.kind
        {
            TirExpr::new(
                TirExprKind::GlobalVarSet {
                    module_source: module_source.clone(),
                    name: name.clone(),
                    value: Box::new(combined),
                },
                TypeTable::UNIT,
                span,
            )
        } else {
            TirExpr::new(
                TirExprKind::Assign {
                    target: Box::new(place),
                    value: Box::new(combined),
                },
                TypeTable::UNIT,
                span,
            )
        };
        self.wrap_prelude(prelude, write, span)
    }

    /// Reify the `?` postfix operator into the `Match` its operand type calls
    /// for: `match expr { Some(v) => v, None => return null }` for `Option<T>`,
    /// `match expr { Ok(v) => v, Err(e) => return Err(From::from(e)) }` for
    /// `Result<T, E>`. Annotate has already validated both the operand and the
    /// function's return type.
    fn reify_question_mark(
        &mut self,
        qm: &ast::TryOpExpr,
        ctx: &mut FunctionContext,
        _recorded_type: TypeId,
    ) -> TirExpr {
        let inner = self.reify_expr(&qm.expr, ctx, None);
        let inner_type = inner.type_id;

        let (is_option, is_result) = {
            let tt = self.tysys.type_table.borrow();
            (
                tt.as_option(inner_type).is_some(),
                matches!(
                    tt.get(inner_type),
                    ResolvedType::GenericInstance { def, .. } if tt.def_name(*def) == "Result"
                ),
            )
        };

        if is_option {
            self.reify_question_mark_option(inner, ctx, qm.span)
        } else if is_result {
            self.reify_question_mark_result(inner, ctx, qm.span, qm.id)
        } else {
            // Annotate already diagnosed; produce a Unit-typed
            // placeholder of `ERROR` so downstream phases see the
            // same shape annotate's recovery path produced.
            TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, qm.span)
        }
    }

    /// `Option<T>`'s `?`-op desugar — mirrors
    /// `Elaborator::resolve_question_mark_option`.
    fn reify_question_mark_option(
        &mut self,
        inner: TirExpr,
        ctx: &mut FunctionContext,
        span: crate::token::Span,
    ) -> TirExpr {
        use crate::tir::{TirBlock, TirExprKind, TirMatchArm, TirPattern, TirStmtKind, TypeTable};

        let inner_type = inner.type_id;
        let (some_type, some_name, none_name) = {
            let tt = self.tysys.type_table.borrow();
            let some_type = tt.as_option(inner_type).unwrap();
            let items = tt.compiler_items();
            (
                some_type,
                items
                    .variant_case_name(crate::compiler_item::CompilerItem::OptionSome)
                    .to_string(),
                items
                    .variant_case_name(crate::compiler_item::CompilerItem::OptionNone)
                    .to_string(),
            )
        };

        ctx.enter_scope();
        let v_local = ctx.add_local("__qm_v".to_string(), some_type, false, None);

        let some_arm = TirMatchArm {
            pattern: TirPattern::Variant {
                enum_type: inner_type,
                variant_name: some_name,
                bindings: vec![TirPattern::Binding {
                    name: "__qm_v".to_string(),
                    local_index: v_local,
                    type_id: some_type,
                }],
                payload_type: some_type,
            },
            guard: None,
            body: TirExpr::new(
                TirExprKind::Local {
                    index: v_local,
                    name: "__qm_v".to_string(),
                },
                some_type,
                span,
            ),
            span,
        };

        let none_arm = TirMatchArm {
            pattern: TirPattern::Variant {
                enum_type: inner_type,
                variant_name: none_name,
                bindings: vec![],
                payload_type: TypeTable::UNIT,
            },
            guard: None,
            body: TirExpr::new(
                TirExprKind::Block(TirBlock::new(
                    vec![
                        self.make_cold_path_stmt(span),
                        TirStmt::new(
                            TirStmtKind::Return {
                                // `Option<U>` and the operand's `Option<T>`
                                // are distinct GC types, so the propagated
                                // `None` is the enclosing function's.
                                value: Some(TirExpr::new(TirExprKind::Null, ctx.return_type, span)),
                            },
                            span,
                        ),
                    ],
                    span,
                )),
                TypeTable::NEVER,
                span,
            ),
            span,
        };

        ctx.exit_scope();

        TirExpr::new(
            TirExprKind::Match {
                expr: Box::new(inner),
                arms: vec![some_arm, none_arm],
            },
            some_type,
            span,
        )
    }

    /// `Result<T, E>`'s `?`-op desugar — mirrors
    /// `Elaborator::resolve_question_mark_result`.
    fn reify_question_mark_result(
        &mut self,
        inner: TirExpr,
        ctx: &mut FunctionContext,
        span: crate::token::Span,
        qm_id: AstId,
    ) -> TirExpr {
        use crate::tir::{
            ResolvedType, TirBlock, TirExprKind, TirMatchArm, TirPattern, TirStmtKind,
        };

        let inner_type = inner.type_id;
        let return_type = ctx.return_type;

        let (ok_type, inner_err_type) = match self.tysys.type_table.borrow().get(inner_type) {
            ResolvedType::GenericInstance { type_args, .. } if type_args.len() == 2 => {
                (type_args[0], type_args[1])
            }
            _ => panic!("reify_question_mark_result: ? operand must be Result<T, E>"),
        };
        let outer_err_type = match self.tysys.type_table.borrow().get(return_type) {
            ResolvedType::GenericInstance { type_args, .. } if type_args.len() == 2 => type_args[1],
            _ => panic!("reify_question_mark_result: ? return type must be Result<U, F>"),
        };

        // When inner and outer error types differ, synthesise a
        // `<OuterErr>::from(<InnerErr>_val)` call. Mirrors
        // `Elaborator::resolve_from_call`; the
        // module source for the impl is looked up via the same
        // search annotate runs (walk impl blocks across loaded
        // modules to find a matching `impl From<InnerErr> for
        // OuterErr`).
        let need_from_conversion = inner_err_type != outer_err_type;

        ctx.enter_scope();
        let v_local = ctx.add_local("__qm_v".to_string(), ok_type, false, None);
        let e_local = ctx.add_local("__qm_e".to_string(), inner_err_type, false, None);

        let (ok_name, err_name, err_index) = {
            let tt = self.tysys.type_table.borrow();
            let items = tt.compiler_items();
            let (_, _, ok_n, _ok_i) =
                items.require_variant_case(crate::compiler_item::CompilerItem::ResultOk);
            let (_, _, err_n, err_i) =
                items.require_variant_case(crate::compiler_item::CompilerItem::ResultErr);
            (ok_n.to_string(), err_n.to_string(), err_i)
        };

        let ok_arm = TirMatchArm {
            pattern: TirPattern::Variant {
                enum_type: inner_type,
                variant_name: ok_name,
                bindings: vec![TirPattern::Binding {
                    name: "__qm_v".to_string(),
                    local_index: v_local,
                    type_id: ok_type,
                }],
                payload_type: ok_type,
            },
            guard: None,
            body: TirExpr::new(
                TirExprKind::Local {
                    index: v_local,
                    name: "__qm_v".to_string(),
                },
                ok_type,
                span,
            ),
            span,
        };

        let e_expr = TirExpr::new(
            TirExprKind::Local {
                index: e_local,
                name: "__qm_e".to_string(),
            },
            inner_err_type,
            span,
        );
        let converted_err = if need_from_conversion {
            self.reify_from_call(outer_err_type, inner_err_type, e_expr, span, qm_id)
        } else {
            e_expr
        };
        let err_variant = TirExpr::new(
            TirExprKind::VariantConstruct {
                variant_type: return_type,
                case_index: err_index,
                case_name: err_name.clone(),
                payload: Some(Box::new(converted_err)),
            },
            return_type,
            span,
        );

        let err_arm = TirMatchArm {
            pattern: TirPattern::Variant {
                enum_type: inner_type,
                variant_name: err_name,
                bindings: vec![TirPattern::Binding {
                    name: "__qm_e".to_string(),
                    local_index: e_local,
                    type_id: inner_err_type,
                }],
                payload_type: inner_err_type,
            },
            guard: None,
            body: TirExpr::new(
                TirExprKind::Block(TirBlock::new(
                    vec![
                        self.make_cold_path_stmt(span),
                        TirStmt::new(
                            TirStmtKind::Return {
                                value: Some(err_variant),
                            },
                            span,
                        ),
                    ],
                    span,
                )),
                crate::tir::TypeTable::NEVER,
                span,
            ),
            span,
        };

        ctx.exit_scope();

        TirExpr::new(
            TirExprKind::Match {
                expr: Box::new(inner),
                arms: vec![ok_arm, err_arm],
            },
            ok_type,
            span,
        )
    }

    /// Reify a comparison chain `a < b < c …` into
    /// `(a < m_0) && (m_0 < m_1) && …` inside a block holding one `__mK` binding
    /// per middle term, so no term is re-evaluated. Non-primitive operands
    /// dispatch through the `operator_dispatch` record on the chain's own
    /// `AstId` — the synthesised inner comparisons have no source id.
    fn reify_comparison_chain(
        &mut self,
        chain: &ast::ComparisonChainExpr,
        ctx: &mut FunctionContext,
    ) -> TirExpr {
        use crate::tir::{TirBinaryOp, TirBlock, TirExprKind, TirStmtKind, TypeTable};

        if chain.comparisons.is_empty() {
            // Degenerate parse — annotate emits `chain.first` as-is.
            return self.reify_expr(&chain.first, ctx, None);
        }

        if chain.comparisons.len() == 1 {
            let cmp = &chain.comparisons[0];
            let (left, right) =
                if super::Elaborator::<H>::takes_shape_from_expected_type(&chain.first)
                    && !super::Elaborator::<H>::takes_shape_from_expected_type(&cmp.right)
                {
                    let right = self.reify_expr(&cmp.right, ctx, None);
                    let left = self.reify_expr(&chain.first, ctx, Some(right.type_id));
                    (left, right)
                } else {
                    let left = self.reify_expr(&chain.first, ctx, None);
                    let right = self.reify_expr(&cmp.right, ctx, Some(left.type_id));
                    (left, right)
                };

            // Non-primitive comparison dispatches through `Eq::eq` /
            // `Ord::cmp`; the recording fires on `chain.id`.
            if let Some(dispatch) = self.ann_operator_dispatch(chain.id) {
                let receiver = adjust_receiver_for_self_kind(
                    left,
                    dispatch.self_kind,
                    /* is_ref_impl */ false,
                    chain.span,
                    &self.tysys.type_table,
                );
                let args = vec![right];
                let call_args: Vec<crate::tir::CallArg> = args
                    .into_iter()
                    .zip(dispatch.arg_ref_wraps.iter().copied())
                    .map(|(arg, wrap)| {
                        let arg_expr = if wrap {
                            let arg_ref_type = self
                                .tysys
                                .type_table
                                .borrow_mut()
                                .intern(crate::tir::ResolvedType::Ref(arg.type_id));
                            TirExpr::new(
                                TirExprKind::Unary {
                                    op: crate::tir::TirUnaryOp::Ref,
                                    expr: Box::new(arg),
                                },
                                arg_ref_type,
                                chain.span,
                            )
                        } else {
                            arg
                        };
                        crate::tir::CallArg::new(arg_expr, false)
                    })
                    .collect();
                let method_call = build_tir_method_call(
                    receiver,
                    dispatch.function_ref,
                    vec![],
                    call_args,
                    dispatch.return_type,
                    chain.span,
                );

                // Ord ops wrap `cmp(...) ==/!= Less/Greater`;
                // `!=` via `Eq::eq` wraps with `!`.
                use ast::BinaryOp;
                if matches!(
                    cmp.op,
                    BinaryOp::Lt | BinaryOp::Gt | BinaryOp::LtEq | BinaryOp::GtEq
                ) {
                    return ord_bool_from_cmp(
                        method_call,
                        cmp.op,
                        chain.span,
                        &self.tysys.type_table,
                    );
                }
                if cmp.op == BinaryOp::NotEq && method_call.type_id == TypeTable::BOOL {
                    return TirExpr::new(
                        TirExprKind::Unary {
                            op: crate::tir::TirUnaryOp::Not,
                            expr: Box::new(method_call),
                        },
                        TypeTable::BOOL,
                        chain.span,
                    );
                }
                return method_call;
            }

            let recorded_type = self
                .ann_expression_types(chain.id)
                .unwrap_or(TypeTable::BOOL);
            return TirExpr::new(
                crate::tir::TirExprKind::Binary {
                    left: Box::new(left),
                    op: ast_binary_op_to_tir(cmp.op),
                    right: Box::new(right),
                },
                recorded_type,
                cmp.op_span,
            );
        }

        ctx.enter_scope();
        let mut stmts: Vec<TirStmt> = Vec::new();

        let cmp0 = &chain.comparisons[0];
        let first_tir = self.reify_expr(&chain.first, ctx, None);
        let right0_tir = self.reify_expr(&cmp0.right, ctx, Some(first_tir.type_id));

        // Bind first middle to `__m0`.
        let m0_type = right0_tir.type_id;
        let m0_name = "__m0".to_string();
        let m0_index = ctx.add_local(m0_name.clone(), m0_type, false, None);
        stmts.push(TirStmt::new(
            TirStmtKind::Let {
                name: m0_name.clone(),
                local_index: m0_index,
                is_mut: false,
                is_reactive: false,
                type_id: m0_type,
                value: right0_tir,
                skip_value_copy: false,
            },
            chain.span,
        ));
        let m0_ref = TirExpr::new(
            TirExprKind::Local {
                index: m0_index,
                name: m0_name,
            },
            m0_type,
            chain.span,
        );

        let mut acc_tir = TirExpr::new(
            crate::tir::TirExprKind::Binary {
                left: Box::new(first_tir),
                op: ast_binary_op_to_tir(cmp0.op),
                right: Box::new(m0_ref.clone()),
            },
            TypeTable::BOOL,
            cmp0.op_span,
        );
        let mut prev_tir = m0_ref;

        let last_idx = chain.comparisons.len() - 1;
        for idx in 1..chain.comparisons.len() {
            let cmp = &chain.comparisons[idx];
            let raw_right = self.reify_expr(&cmp.right, ctx, Some(prev_tir.type_id));
            let right_tir = if idx == last_idx {
                raw_right
            } else {
                let m_type = raw_right.type_id;
                let m_name = format!("__m{idx}");
                let m_index = ctx.add_local(m_name.clone(), m_type, false, None);
                stmts.push(TirStmt::new(
                    TirStmtKind::Let {
                        name: m_name.clone(),
                        local_index: m_index,
                        is_mut: false,
                        is_reactive: false,
                        type_id: m_type,
                        value: raw_right,
                        skip_value_copy: false,
                    },
                    chain.span,
                ));
                TirExpr::new(
                    TirExprKind::Local {
                        index: m_index,
                        name: m_name,
                    },
                    m_type,
                    chain.span,
                )
            };
            let next_prev = right_tir.clone();
            let cmp_tir = TirExpr::new(
                crate::tir::TirExprKind::Binary {
                    left: Box::new(prev_tir),
                    op: ast_binary_op_to_tir(cmp.op),
                    right: Box::new(right_tir),
                },
                TypeTable::BOOL,
                cmp.op_span,
            );
            acc_tir = TirExpr::new(
                crate::tir::TirExprKind::Binary {
                    left: Box::new(acc_tir),
                    op: TirBinaryOp::And,
                    right: Box::new(cmp_tir),
                },
                TypeTable::BOOL,
                chain.span,
            );
            prev_tir = next_prev;
        }

        ctx.exit_scope();

        stmts.push(TirStmt::new(TirStmtKind::Expr(acc_tir), chain.span));
        TirExpr::new(
            TirExprKind::Block(TirBlock::new(stmts, chain.span)),
            TypeTable::BOOL,
            chain.span,
        )
    }

    /// Reify an `expr[idx]` index expression into one of three shapes: a tuple
    /// constant index becomes a `FieldAccess`, an `Index` dispatch becomes
    /// `*receiver.index(idx)` (the record's `Ref(Output)` return type is what
    /// signals the `Deref` wrap), and an `IndexValue` dispatch becomes
    /// `receiver.index_value(idx)` with no wrap.
    fn reify_index(
        &mut self,
        index: &ast::IndexExpr,
        ctx: &mut FunctionContext,
        recorded_type: TypeId,
    ) -> TirExpr {
        use crate::tir::{ResolvedType, TirExprKind, TypeTable};

        let receiver = self.reify_expr(&index.expr, ctx, None);

        // Tuple constant-index path: detect via the receiver's
        // resolved type + the index being a constant integer
        // literal. Matches `Elaborator::resolve_index`'s tuple
        // branch.
        let tuple_elems: Option<Vec<TypeId>> = {
            let tt = self.tysys.type_table.borrow();
            let base = receiver.type_id;
            let unwrapped = match tt.get(base) {
                ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => *inner,
                _ => base,
            };
            tt.as_tuple(unwrapped)
        };
        if let Some(elems) = &tuple_elems
            && let ast::Expr::Literal(lit) = &index.index
            && let ast::Literal::Number(repr) = &lit.value
            && let Ok(idx) = repr.parse::<usize>()
            && let Ok(elem) =
                super::Elaborator::<H>::tuple_literal_index_type(&self.tysys.type_table, elems, idx)
        {
            return TirExpr::new(
                TirExprKind::FieldAccess {
                    expr: Box::new(receiver),
                    field_index: idx as u32,
                    field_name: idx.to_string(),
                },
                elem,
                index.span,
            );
        }

        // Pack-typed tuple subscripted by an enclosing variadic
        // `.enumerate()` index: kept as `Index` here and rewritten to the
        // element's `FieldAccess` when the loop unrolls (WEP 2026-03-14).
        if let Some(elems) = &tuple_elems
            && let Some(elem_type) = super::Elaborator::<H>::variadic_enumerate_subscript_type(
                &self.tysys.type_table,
                elems,
                &index.index,
                ctx,
            )
        {
            let idx_expr = self.reify_expr(&index.index, ctx, None);
            return TirExpr::new(
                TirExprKind::Index {
                    expr: Box::new(receiver),
                    index: Box::new(idx_expr),
                },
                elem_type,
                index.span,
            );
        }

        // Operator-dispatch path: `operator_dispatch[index.id]` carries the
        // resolved Index / IndexValue method and `needs_deref`. Shared with the
        // compound-assign read via `build_index_read_from_dispatch`.
        if let Some(dispatch) = self.ann_operator_dispatch(index.id) {
            let idx_expr = self.reify_expr(&index.index, ctx, None);
            let deref_type = self.index_deref_type(Some(recorded_type), &dispatch);
            return self.build_index_read_from_dispatch(
                receiver, idx_expr, dispatch, deref_type, index.span,
            );
        }

        // No dispatch recorded → the elaborator emitted a recovery
        // shape (annotate would have diagnosed missing trait impl).
        // Match the recovery output with a Unit placeholder typed
        // as ERROR.
        let _ = recorded_type;
        TirExpr::new(TirExprKind::Unit, TypeTable::ERROR, index.span)
    }

    /// Reify a closure expression from the capture analysis in
    /// `sem.types.closure_captures[closure.id]`: `mut_captures` materialise as
    /// `let __ref_v = &mut v;` ahead of the body in declaration order, `captures`
    /// is the final capture list, and `is_mutating` picks `fn mut(…)`. Follows
    /// `resolve_closure` step by step so the walk-order invariant holds.
    fn reify_closure(
        &mut self,
        closure: &ast::ClosureExpr,
        ctx: &mut FunctionContext,
        recorded_type: TypeId,
        expected_type: Option<TypeId>,
    ) -> TirExpr {
        use crate::tir::{
            ResolvedType, TirBlock, TirCapture, TirExprKind, TirStmtKind, TirUnaryOp, TypeTable,
        };

        let span = closure.span;

        let cap_info = self
            .ann_closure_captures(closure.id)
            .expect("resolve_closure records the capture info for every closure reify emits");

        // Step 1 (replay): materialise outer-scope `__ref_v` locals
        // for each mut-capture in the recorded order; emit the
        // matching `let __ref_v = &mut v;` TIR; register
        // `deref_overrides` so the closure body's references to
        // captured mut-locals dereference the proxy.
        let mut ref_stmts: Vec<TirStmt> = Vec::new();
        let mut deref_overrides: crate::hashmap::IndexMap<String, (String, TypeId)> =
            crate::hashmap::IndexMap::default();
        for mc in &cap_info.mut_captures {
            // The slot comes from this walk; the `&mut` goes to the reserved
            // index, which is the one the capture list records.
            ctx.add_local(mc.ref_name.clone(), mc.ref_type, false, None);
            let ref_index = mc.ref_index;
            ctx.address_taken_locals.insert(mc.outer_index);
            ref_stmts.push(TirStmt::new(
                TirStmtKind::Let {
                    name: mc.ref_name.clone(),
                    local_index: ref_index,
                    is_mut: false,
                    is_reactive: false,
                    type_id: mc.ref_type,
                    value: TirExpr::new(
                        TirExprKind::Unary {
                            op: TirUnaryOp::MutRef,
                            expr: Box::new(TirExpr::new(
                                TirExprKind::Local {
                                    index: mc.outer_index,
                                    name: mc.var_name.clone(),
                                },
                                mc.inner_type,
                                span,
                            )),
                        },
                        mc.ref_type,
                        span,
                    ),
                    skip_value_copy: false,
                },
                span,
            ));
            deref_overrides.insert(mc.var_name.clone(), (mc.ref_name.clone(), mc.inner_type));
        }

        // Step 2: open the closure context with the deref overrides.
        let mut closure_ctx =
            FunctionContext::new_closure(TypeTable::UNKNOWN, ctx, &self.tysys.type_table);
        closure_ctx.deref_overrides = deref_overrides;

        // Step 3: add closure parameters. Their types come from `local_types`,
        // which `resolve_closure` populated per param `AstId` — reify is a pure
        // read here. The expected fn type, peeled through newtypes so a closure
        // coerced to `type Reducer = fn(..)` sees the underlying signature,
        // feeds the return type only (Step 4).
        let expected_fn_type = expected_type.map(|t| {
            let table = self.tysys.type_table.borrow();
            table.representation_head(table.peel_refs(t))
        });
        let params: Vec<(String, TypeId)> = closure
            .params
            .iter()
            .map(|p| {
                let type_id = self.ann_local_type(p.id).unwrap_or(TypeTable::UNKNOWN);
                closure_ctx.add_local_at(
                    p.name.clone(),
                    type_id,
                    p.is_mut,
                    Some(p.id),
                    p.name_span,
                );
                (p.name.clone(), type_id)
            })
            .collect();

        // Step 4: reify the body in the closure scope.
        // An explicit `|params| -> Type ...` annotation is the authoritative
        // return type; otherwise use the expected fn type's return. The
        // annotation was resolved by `resolve_closure` in the scope it was
        // written in — re-resolving it here has no `Self` to bind.
        let declared_return = cap_info.declared_return;
        let body_expected = declared_return.or_else(|| {
            expected_fn_type
                .and_then(|t| match self.tysys.type_table.borrow().get(t) {
                    ResolvedType::Function { return_type, .. } => Some(*return_type),
                    _ => None,
                })
                // A rigid parameter belongs to the signature this call is
                // instantiating, and the closure's own body is what determines
                // it — seeding the body with it would demand the body produce
                // an opaque type it cannot construct. Mirrors `resolve_closure`.
                .filter(|&rt| {
                    !matches!(
                        self.tysys.type_table.borrow().get(rt),
                        ResolvedType::TypeParam { .. }
                    )
                })
        });

        // A block body with explicit `return X` has a NEVER/UNIT tail, so its
        // logical return type comes from the returned expressions, which
        // annotate already recorded (independent of the body TIR). Compute it
        // now, before reifying the body: a `return X` statement reifies its
        // value against `ctx.return_type` (see the `ast::Stmt::Return` arm), so
        // leaving it UNKNOWN makes a bare `return null` emit a nullref against
        // the non-null `(ref $Option)` slot another arm's `return
        // Option::Some(..)` fixes — an invalid closure. Prefer an explicit
        // expected fn return type; fall back to the block-return type.
        let block_return_type = if let crate::ast::Expr::Block(ref block) = closure.body {
            let ctrl_ctx = super::control_flow::CtrlFlowCtx {
                expression_types: &self.sem.types.expression_types,
                type_table: &self.tysys.type_table,
            };
            super::control_flow::find_return_type_in_block(ctrl_ctx, block)
        } else {
            None
        };
        if let Some(rt) = body_expected.or(block_return_type) {
            closure_ctx.return_type = rt;
        }

        let body = self.reify_expr(&closure.body, &mut closure_ctx, body_expected);

        // Step 5: assemble the capture list from the recorded entries.
        let captures: Vec<TirCapture> = cap_info
            .captures
            .iter()
            .map(|c| TirCapture {
                name: c.name.clone(),
                outer_index: c.outer_index,
                type_id: c.type_id,
                is_mut: c.is_mut,
            })
            .collect();

        // An explicit annotation wins; otherwise single-expression closure
        // bodies (e.g. `|c| c.method()`) take their body's type as the return
        // type directly, and block bodies use the return-expression type
        // computed above.
        // `block_return_type` reads `expression_types`, which a site that
        // elaborated twice leaves empty for the committed pass; fall back to
        // the body TIR, which is built by now either way.
        let return_type = declared_return
            .or(block_return_type)
            .or_else(|| tir_block_return_type(&body))
            .unwrap_or(body.type_id);

        let param_types: Vec<TypeId> = params.iter().map(|(_, t)| *t).collect();
        let func_type = self.tysys.type_table.borrow_mut().make_function_with_mut(
            cap_info.is_mutating,
            param_types,
            return_type,
            Vec::new(),
            Vec::new(),
        );

        let mut all_locals = closure_ctx.locals;
        let body_locals = if params.len() <= all_locals.len() {
            all_locals.split_off(params.len())
        } else {
            Vec::new()
        };
        let address_taken_locals = closure_ctx.address_taken_locals;

        let declared_effects =
            expected_type.and_then(|t| match self.tysys.type_table.borrow().get(t) {
                ResolvedType::Function { effects, .. } if !effects.is_empty() => {
                    Some(effects.clone())
                }
                _ => None,
            });

        let closure_tir = TirExpr::new(
            TirExprKind::Closure {
                params,
                body: Box::new(body),
                captures,
                functor_id: None,
                address_taken_locals,
                body_locals,
                declared_effects,
            },
            func_type,
            span,
        );

        // Step 7: wrap in a Block when ref_stmts materialised any
        // outer-scope `__ref_v` bindings.
        if ref_stmts.is_empty() {
            let _ = recorded_type;
            return closure_tir;
        }

        let mut stmts = ref_stmts;
        stmts.push(TirStmt::new(TirStmtKind::Expr(closure_tir), span));
        TirExpr::new(
            TirExprKind::Block(TirBlock::new(stmts, span)),
            func_type,
            span,
        )
    }

    /// True when `type_id` is a `TypePack` or a tuple whose elements
    /// transitively contain one.
    fn type_contains_pack(&self, type_id: TypeId) -> bool {
        use crate::tir::{ResolvedType, TypeTable};
        let ty = self.tysys.type_table.borrow().get(type_id).clone();
        match ty {
            ResolvedType::TypePack { .. } => true,
            ResolvedType::GenericInstance { def, type_args }
                if TypeTable::is_tuple_type(self.tysys.type_table.borrow().def_name(def)) =>
            {
                type_args.iter().any(|e| self.type_contains_pack(*e))
            }
            _ => false,
        }
    }

    /// Reify a tuple literal, handling spread elements. The tuple `TypeId` is
    /// built bottom-up via `make_tuple` so a nested tuple's element type is the
    /// same interned id as the inner literal's, which `nir/sroa` relies on. A
    /// spread expands per `type_contains_pack`: a pack to `TypePackExpansion`, a
    /// tuple containing one to `TupleSpread`, a concrete one to `FieldAccess`es.
    fn reify_tuple_literal(
        &mut self,
        tuple_lit: &ast::TupleLiteralExpr,
        ctx: &mut FunctionContext,
        span: crate::token::Span,
    ) -> TirExpr {
        use crate::tir::{
            ResolvedType, TirBlock, TirExpr, TirExprKind, TirStmt, TirStmtKind, TypeTable,
        };

        let mut elements: Vec<TirExpr> = Vec::new();
        let mut elem_types: Vec<TypeId> = Vec::new();
        // (local_idx, name, expr, span) for non-trivial spread operands.
        let mut spread_bindings: Vec<(u32, String, TirExpr, crate::token::Span)> = Vec::new();

        for elem in &tuple_lit.elements {
            if let ast::Expr::Spread(inner, _span) = elem {
                let spread_expr = self.reify_expr(inner, ctx, None);
                let contains_pack = self.type_contains_pack(spread_expr.type_id);
                let spread_type = self
                    .tysys
                    .type_table
                    .borrow()
                    .get(spread_expr.type_id)
                    .clone();
                if contains_pack {
                    let is_direct_pack = matches!(
                        self.tysys.type_table.borrow().get(spread_expr.type_id),
                        ResolvedType::TypePack { .. }
                    );
                    if is_direct_pack {
                        let pack_type_id = spread_expr.type_id;
                        elem_types.push(spread_expr.type_id);
                        elements.push(TirExpr::new(
                            TirExprKind::TypePackExpansion {
                                call_expr: Box::new(spread_expr),
                                pack_type_id,
                            },
                            *elem_types.last().unwrap(),
                            elem.span(),
                        ));
                    } else {
                        elem_types.push(spread_expr.type_id);
                        elements.push(TirExpr::new(
                            TirExprKind::TupleSpread {
                                expr: Box::new(spread_expr),
                            },
                            *elem_types.last().unwrap(),
                            elem.span(),
                        ));
                    }
                } else if let Some((pack_name, pack_index)) =
                    self.ann_pack_spread_subject(inner.id())
                {
                    // Pack-map `..F::method()` (pack-independent return): the
                    // expansion iterates the *plain* pack `F` to rewrite the call
                    // per field type; the element type is the mapped pack, which
                    // splices to the return type repeated `|F|` times.
                    let plain_pack = self
                        .tysys
                        .type_table
                        .borrow_mut()
                        .make_type_pack(pack_name.clone(), pack_index);
                    let mapped = self.tysys.type_table.borrow_mut().make_mapped_type_pack(
                        pack_name,
                        pack_index,
                        spread_expr.type_id,
                    );
                    elem_types.push(mapped);
                    elements.push(TirExpr::new(
                        TirExprKind::TypePackExpansion {
                            call_expr: Box::new(spread_expr),
                            pack_type_id: plain_pack,
                        },
                        mapped,
                        elem.span(),
                    ));
                } else if let ResolvedType::GenericInstance {
                    def,
                    type_args: inner_elems,
                } = spread_type
                    && TypeTable::is_tuple_type(self.tysys.type_table.borrow().def_name(def))
                {
                    // Concrete tuple: expand inline via FieldAccess. Bind a
                    // non-trivial operand to a temporary for single evaluation.
                    let spread_ref = if matches!(spread_expr.kind, TirExprKind::Local { .. }) {
                        spread_expr
                    } else {
                        let spread_type_id = spread_expr.type_id;
                        let tmp_name = format!("__spread_{}", ctx.next_local);
                        let tmp_idx = ctx.add_local(tmp_name.clone(), spread_type_id, false, None);
                        spread_bindings.push((tmp_idx, tmp_name.clone(), spread_expr, elem.span()));
                        TirExpr::new(
                            TirExprKind::Local {
                                index: tmp_idx,
                                name: tmp_name,
                            },
                            spread_type_id,
                            elem.span(),
                        )
                    };
                    for (i, &et) in inner_elems.iter().enumerate() {
                        elements.push(TirExpr::new(
                            TirExprKind::FieldAccess {
                                expr: Box::new(spread_ref.clone()),
                                field_index: i as u32,
                                field_name: i.to_string(),
                            },
                            et,
                            elem.span(),
                        ));
                        elem_types.push(et);
                    }
                } else {
                    // A stray spread of a non-tuple — annotate already
                    // diagnosed it; pass the operand through unchanged.
                    elem_types.push(spread_expr.type_id);
                    elements.push(spread_expr);
                }
            } else {
                let resolved = self.reify_expr(elem, ctx, None);
                elem_types.push(resolved.type_id);
                elements.push(resolved);
            }
        }

        let tuple_type = self.tysys.type_table.borrow_mut().make_tuple(elem_types);
        let tuple_expr = TirExpr::new(TirExprKind::TupleLiteral { elements }, tuple_type, span);

        if spread_bindings.is_empty() {
            tuple_expr
        } else {
            let mut stmts: Vec<TirStmt> = spread_bindings
                .into_iter()
                .map(|(idx, name, value, span)| {
                    let type_id = value.type_id;
                    TirStmt::new(
                        TirStmtKind::Let {
                            name,
                            local_index: idx,
                            value,
                            is_mut: false,
                            is_reactive: false,
                            type_id,
                            skip_value_copy: false,
                        },
                        span,
                    )
                })
                .collect();
            stmts.push(TirStmt::new(TirStmtKind::Expr(tuple_expr), span));
            let block = TirBlock::new(stmts, span);
            TirExpr::new(TirExprKind::Block(block), tuple_type, span)
        }
    }

    /// Reify a `[e0, e1, …]` literal coerced through `From<Array<E>>` (WEP
    /// 2026-08-24): materialize the array, hand it to `from`, and cast the
    /// result when the target is a newtype over what `from` returns.
    fn reify_sequence_coercion(
        &mut self,
        tuple_lit: &ast::TupleLiteralExpr,
        facts: super::sem::types::SequenceCoercionFacts,
        ctx: &mut FunctionContext,
        span: crate::token::Span,
    ) -> TirExpr {
        let elements = tuple_lit
            .elements
            .iter()
            .map(|element| self.reify_literal_element(element, facts.element_type, ctx))
            .collect();
        let array = TirExpr::new(
            crate::tir::TirExprKind::ArrayLiteral { elements },
            facts.call.from_type,
            span,
        );
        let built = build_literal_from_call(array, &facts.call, span);
        cast_to_newtype(built, facts.newtype_cast_to, span)
    }

    /// Reify a `{ k: v, … }` literal coerced through `From<Array<[K, V]>>`
    /// (WEP 2026-08-24). Without a spread it is one `from` call over the pair
    /// array; with one it is a left-to-right fold, a run of consecutive `k: v`
    /// members per `from` call and one `spread_literal` per `..base`.
    fn reify_key_value_coercion(
        &mut self,
        struct_lit: &ast::StructLiteralExpr,
        facts: super::sem::types::KeyValueCoercionFacts,
        ctx: &mut FunctionContext,
        span: crate::token::Span,
    ) -> TirExpr {
        use crate::tir::{TirBlock, TirExprKind, TirStmt, TirStmtKind};

        let string_type = self
            .tysys
            .type_table
            .borrow_mut()
            .make_compiler_struct(crate::compiler_item::CompilerItem::String);

        let output_type = facts.call.output_type;
        let cast = |built: TirExpr| cast_to_newtype(built, facts.newtype_cast_to, span);

        if struct_lit.spreads.is_empty() {
            let pairs = struct_lit
                .fields
                .iter()
                .map(|field| self.reify_kv_pair(field, &facts, string_type, ctx))
                .collect();
            return cast(build_kv_from_call(pairs, &facts, span));
        }

        // `{ ..a, x: 1, ..b }` — members in source order, last write wins. The
        // accumulator is seeded with the first member so the common
        // `{ ..base, … }` costs one copy and one merge.
        let label = "__kv_lit".to_string();
        ctx.enter_scope();
        let mut stmts: Vec<TirStmt> = Vec::new();
        let mut acc: Option<u32> = None;
        let mut run: Vec<TirExpr> = Vec::new();
        let mut members: Vec<(TirExpr, crate::token::Span)> = Vec::new();
        for member in struct_lit.members() {
            match member {
                ast::LiteralMember::Spread(_, spread) => {
                    if !run.is_empty() {
                        let pairs = std::mem::take(&mut run);
                        members.push((build_kv_from_call(pairs, &facts, span), span));
                    }
                    let base = self.reify_expr(&spread.expr, ctx, Some(output_type));
                    members.push((base, spread.span));
                }
                ast::LiteralMember::Field(_, field) => {
                    run.push(self.reify_kv_pair(field, &facts, string_type, ctx));
                }
            }
        }
        if !run.is_empty() {
            members.push((build_kv_from_call(run, &facts, span), span));
        }
        for (value, member_span) in members {
            match acc {
                None => {
                    let index = ctx.add_local("__acc".to_string(), output_type, true, None);
                    stmts.push(TirStmt::new(
                        TirStmtKind::Let {
                            name: "__acc".to_string(),
                            local_index: index,
                            is_mut: true,
                            is_reactive: false,
                            type_id: output_type,
                            value,
                            skip_value_copy: false,
                        },
                        member_span,
                    ));
                    acc = Some(index);
                }
                Some(index) => {
                    let spread = facts.spread.as_ref().expect(
                        "annotate reports a literal whose target has no `LiteralSpread` impl",
                    );
                    let call =
                        build_literal_spread_call(index, output_type, value, spread, member_span);
                    stmts.push(TirStmt::new(TirStmtKind::Expr(call), member_span));
                }
            }
        }
        let index = acc.expect("a literal with a spread has at least one member");
        stmts.push(TirStmt::new(
            TirStmtKind::Break {
                label: Some(label.clone()),
                value: Some(TirExpr::new(
                    TirExprKind::Local {
                        index,
                        name: "__acc".to_string(),
                    },
                    output_type,
                    span,
                )),
            },
            span,
        ));
        ctx.exit_scope();

        cast(TirExpr::new(
            TirExprKind::LabeledBlock {
                label,
                block: TirBlock::new(stmts, span),
                result_type: output_type,
            },
            output_type,
            span,
        ))
    }

    /// Reify one literal element, converting it into its slot's type through
    /// the `From` annotate recorded (WEP 2026-08-24). Without a recorded
    /// conversion the element already has the slot's type.
    fn reify_literal_element(
        &mut self,
        element: &ast::Expr,
        slot_type: crate::tir::TypeId,
        ctx: &mut FunctionContext,
    ) -> TirExpr {
        let conversion = self.ann_literal_conversions(element.id());
        let expected = conversion.as_ref().map_or(slot_type, |c| c.from_type);
        let value = self.reify_expr(element, ctx, Some(expected));
        match conversion {
            Some(call) => build_literal_from_call(value, &call, element.span()),
            None => value,
        }
    }

    /// One `[key, value]` pair of a key-value literal.
    fn reify_kv_pair(
        &mut self,
        field: &ast::StructLiteralField,
        facts: &super::sem::types::KeyValueCoercionFacts,
        string_type: crate::tir::TypeId,
        ctx: &mut FunctionContext,
    ) -> TirExpr {
        let key = TirExpr::new(
            crate::tir::TirExprKind::StringLiteral(field.name.clone()),
            string_type,
            field.name_span,
        );
        let value = self.reify_literal_element(&field.value, facts.value_type, ctx);
        TirExpr::new(
            crate::tir::TirExprKind::TupleLiteral {
                elements: vec![key, value],
            },
            facts.pair_type,
            field.span,
        )
    }

    /// Synthesise a `From<From>::from(value)` call from the facts the
    /// body walk's [`super::Elaborator::resolve_from_call`] recorded
    /// under `caller_id`. Every caller has a source-level handle (`?`
    /// operator, `Type::from(x)` static call, `Type::<…>::from(x)`); the
    /// recording is unconditional, so reify is a pure read.
    fn reify_from_call(
        &mut self,
        target_type: TypeId,
        from_type: TypeId,
        value: TirExpr,
        span: crate::token::Span,
        caller_id: AstId,
    ) -> TirExpr {
        use crate::name::LocalMethodName;
        use crate::tir::{CallArg, FunctionRef, TirExprKind};

        let _ = from_type;
        let facts = self.ann_from_call_facts(caller_id).expect(
            "resolve_from_call records FromCallFacts at every site reify hits — \
                 ?-op, Type::from(x), and Type::<…>::from(x)",
        );
        let from_trait = facts.from_trait_name.with_args(vec![facts.from_name]);

        TirExpr::new(
            TirExprKind::Call {
                func: Box::new(FunctionRef {
                    module_source: facts.module_source,
                    name: facts.mangled_name,
                    monomorph_info: None,
                    method_info: Some(LocalMethodName {
                        receiver: Receiver::Type(facts.target_name),
                        struct_type_args: Vec::new(),
                        trait_name: Some(from_trait),
                        trait_type_args: vec![],
                        method_name: "from".to_string(),
                        method_type_args: vec![],
                        is_type_param_receiver: false,
                        is_ref_impl: false,
                        cm_name: None,
                    }),
                }),
                type_args: vec![],
                args: vec![CallArg::new(value, false)],
                has_receiver: false,
            },
            target_type,
            span,
        )
    }

    /// Reify a `with E => h, … do { body }` effect handler block.
    /// Mirrors `Elaborator::resolve_with_handler`.
    ///
    /// Both binding forms — explicit `Effect => handler_expr` and bundled
    /// `handler_expr` — read their effect list off
    /// `sem.types.handler_bindings`, so reify enumerates them without
    /// re-running `trait_env.implements_effect`.
    fn reify_with_handler(
        &mut self,
        with_expr: &ast::WithHandlerExpr,
        ctx: &mut FunctionContext,
        result_type: crate::tir::TypeId,
    ) -> TirExpr {
        use crate::tir::{EffectRef, TirExprKind, TirHandlerBinding};

        let mut bindings: Vec<TirHandlerBinding> = Vec::with_capacity(with_expr.handlers.len());
        for binding in &with_expr.handlers {
            // Annotate recorded the binding's effect
            // enumeration on `sem.types.handler_bindings`. Reify
            // reifies the handler expression and stitches one
            // `TirHandlerBinding` per recorded effect entry.
            let binding_key = binding.id;
            let Some(facts) = self.ann_handler_bindings(binding_key) else {
                // Annotate didn't record this binding — either it
                // bailed (diagnosed type) or the binding shape is
                // unsupported; skip to mirror the elaborator's
                // recovery.
                continue;
            };
            let handler = self.reify_expr(&binding.handler, ctx, None);
            for entry in &facts.effects {
                bindings.push(TirHandlerBinding {
                    effect: Some(EffectRef::Concrete {
                        name: entry.name.clone(),
                        module_source: entry.module_source.clone(),
                    }),
                    trait_type_args: entry.trait_type_args.clone(),
                    handler: handler.clone(),
                    handler_type: facts.handler_type,
                    span: binding.span,
                    bundle_group: facts.bundle_group,
                });
            }
        }

        ctx.enter_scope();
        // `with ... do { ... }` evaluates to its body block's trailing value.
        // Propagate the recorded result type so the body's tail expression
        // replays any coercion (e.g. literal widening) the annotate phase
        // applied against the binding's expected type.
        let body = self.reify_block(&with_expr.body, ctx, Some(result_type));
        ctx.exit_scope();

        TirExpr::new(
            TirExprKind::WithHandler {
                bindings,
                body,
                result_type,
            },
            result_type,
            with_expr.span,
        )
    }

    /// Reify a `matches!`-style expression: `scrutinee matches { PAT
    /// [if guard] }`. Desugars (tagged `DesugarKind::Matches` at
    /// annotate time) into a two-arm match: pattern → true, wildcard
    /// → false. Mirror `Elaborator::desugar_matches_expr`.
    fn reify_matches(&mut self, m: &ast::MatchesExpr, ctx: &mut FunctionContext) -> TirExpr {
        use crate::tir::{TirExprKind, TirMatchArm, TirPattern, TypeTable};

        let scrutinee = self.reify_expr(&m.expr, ctx, None);
        let scrutinee_type = scrutinee.type_id;

        ctx.enter_scope();
        let pattern_tir = self.reify_pattern(&m.pattern, scrutinee_type, ctx);
        let arm_body = match &m.guard {
            Some(guard) => self.reify_expr(guard, ctx, Some(TypeTable::BOOL)),
            None => TirExpr::new(TirExprKind::BoolLiteral(true), TypeTable::BOOL, m.span),
        };
        ctx.exit_scope();

        let arms = vec![
            TirMatchArm {
                pattern: pattern_tir,
                guard: None,
                body: arm_body,
                span: m.span,
            },
            TirMatchArm {
                pattern: TirPattern::Wildcard,
                guard: None,
                body: TirExpr::new(TirExprKind::BoolLiteral(false), TypeTable::BOOL, m.span),
                span: m.span,
            },
        ];

        TirExpr::new(
            TirExprKind::Match {
                expr: Box::new(scrutinee),
                arms,
            },
            TypeTable::BOOL,
            m.span,
        )
    }

    /// A struct value's `(name, concrete type, declared index)` fields. Mirrors
    /// `Elaborator::spread_struct_fields` so reify's union plan matches resolve's.
    fn spread_base_field_list(&self, type_id: TypeId) -> Vec<(String, TypeId, u32)> {
        let Some((head, type_args)) =
            super::expr::peel_to_struct(&self.tysys.type_table.borrow(), type_id)
        else {
            return Vec::new();
        };
        let raw: Vec<(String, TypeId, u32)> = {
            let lookup = self.type_lookup();
            let Some(info) = lookup.struct_fields_of_head(head) else {
                return Vec::new();
            };
            info.fields
                .iter()
                .enumerate()
                .map(|(i, (fname, fty, _vis))| (fname.clone(), *fty, i as u32))
                .collect()
        };
        let subst: crate::hashmap::IndexMap<u32, TypeId> = (0..type_args.len() as u32)
            .zip(type_args.iter().copied())
            .collect();
        raw.into_iter()
            .map(|(fname, fty, i)| {
                let concrete = self
                    .tysys
                    .type_table
                    .borrow_mut()
                    .substitute_type_params(fty, &subst);
                (fname, concrete, i)
            })
            .collect()
    }

    /// Reify an anonymous struct literal `{ x: 1, y: 2 }`. Annotate
    /// synthesises the struct from the field shape, gives it a
    /// deterministic `__anon_{x:i32,y:i32}`-style name, and registers
    /// it on `tysys.type_table` + `sem.decls.local_struct_fields` +
    /// `sem.decls.pending_anonymous_structs`. Reify reproduces the
    /// same name from the reified field types and looks the struct
    /// type up; the registration already happened during annotate so
    /// reify is a pure read.
    fn reify_anonymous_struct_literal(
        &mut self,
        struct_lit: &ast::StructLiteralExpr,
        ctx: &mut FunctionContext,
        recorded_type: TypeId,
    ) -> TirExpr {
        use super::expr::UnionSource;
        use crate::tir::{TirBlock, TirExprKind, TirStmt, TirStmtKind, TirStructField};

        // `resolve_anonymous_struct_literal` records the synthesised `__anon_{…}`
        // name (and the union flag) on the `GenericInstantiation` slot.
        let struct_type = recorded_type;
        let (struct_name, is_union) = self
            .ann_generic_instantiations(struct_lit.id)
            .and_then(|gi| gi.mangled_name.map(|name| (name, gi.is_union)))
            .expect("every anonymous struct literal records its synthesised name");

        // Only a composition projects from spread bases; otherwise the explicit
        // fields are the shape.
        if !is_union {
            let fields: Vec<TirStructField> = struct_lit
                .fields
                .iter()
                .enumerate()
                .map(|(index, field)| TirStructField {
                    name: field.name.clone(),
                    value: self.reify_expr(&field.value, ctx, None),
                    field_index: index as u32,
                })
                .collect();
            return TirExpr::new(
                TirExprKind::StructLiteral {
                    struct_type,
                    struct_name,
                    fields,
                },
                struct_type,
                struct_lit.span,
            );
        }

        // Evaluate each member once in source order, hoisting non-trivial ones to
        // temporaries so effects fire in source order regardless of the union's
        // field layout, then assemble the union from the last contributor.
        let mut stmts: Vec<TirStmt> = Vec::new();
        let mut base_refs: Vec<Option<TirExpr>> = vec![None; struct_lit.spreads.len()];
        let mut base_types: Vec<TypeId> =
            vec![crate::tir::TypeTable::UNKNOWN; struct_lit.spreads.len()];
        let mut explicit_refs: Vec<Option<TirExpr>> = vec![None; struct_lit.fields.len()];
        let mut explicit_types: Vec<TypeId> =
            vec![crate::tir::TypeTable::UNKNOWN; struct_lit.fields.len()];
        for member in struct_lit.members() {
            match member {
                ast::LiteralMember::Spread(si, sp) => {
                    let expr = self.reify_expr(&sp.expr, ctx, None);
                    base_types[si] = expr.type_id;
                    base_refs[si] = Some(self.hoist_once(ctx, expr, "__base", &mut stmts));
                }
                ast::LiteralMember::Field(pos, f) => {
                    let expr = self.reify_expr(&f.value, ctx, None);
                    explicit_types[pos] = expr.type_id;
                    explicit_refs[pos] = Some(self.hoist_once(ctx, expr, "__fld", &mut stmts));
                }
            }
        }

        let base_field_lists: Vec<Vec<(String, TypeId, u32)>> = base_types
            .iter()
            .map(|&t| self.spread_base_field_list(t))
            .collect();
        let plan = super::expr::compose_union_plan(struct_lit, &base_field_lists, &explicit_types);
        let fields: Vec<TirStructField> = plan
            .iter()
            .enumerate()
            .map(|(i, uf)| {
                let value = match uf.source {
                    UnionSource::Explicit(idx) => explicit_refs[idx]
                        .clone()
                        .expect("every explicit field is reified above"),
                    UnionSource::Base {
                        base_idx,
                        field_index,
                    } => TirExpr::new(
                        TirExprKind::FieldAccess {
                            expr: Box::new(
                                base_refs[base_idx]
                                    .clone()
                                    .expect("every spread base is reified above"),
                            ),
                            field_index,
                            field_name: uf.name.clone(),
                        },
                        uf.type_id,
                        struct_lit.span,
                    ),
                };
                TirStructField {
                    name: uf.name.clone(),
                    value,
                    field_index: i as u32,
                }
            })
            .collect();

        let literal = TirExpr::new(
            TirExprKind::StructLiteral {
                struct_type,
                struct_name,
                fields,
            },
            struct_type,
            struct_lit.span,
        );

        if stmts.is_empty() {
            return literal;
        }
        stmts.push(TirStmt::new(TirStmtKind::Expr(literal), struct_lit.span));
        TirExpr::new(
            TirExprKind::Block(TirBlock::new(stmts, struct_lit.span)),
            struct_type,
            struct_lit.span,
        )
    }

    /// Bind `expr` to a fresh `{prefix}_N` temporary (pushed onto `stmts`) so it
    /// evaluates once in place, unless it is already a local. Returns a reference.
    fn hoist_once(
        &mut self,
        ctx: &mut FunctionContext,
        expr: TirExpr,
        prefix: &str,
        stmts: &mut Vec<crate::tir::TirStmt>,
    ) -> TirExpr {
        use crate::tir::{TirExprKind, TirStmt, TirStmtKind};
        if matches!(expr.kind, TirExprKind::Local { .. }) {
            return expr;
        }
        let span = expr.span;
        let type_id = expr.type_id;
        let name = format!("{prefix}_{}", ctx.next_local);
        let index = ctx.add_local(name.clone(), type_id, false, None);
        stmts.push(TirStmt::new(
            TirStmtKind::Let {
                name: name.clone(),
                local_index: index,
                value: expr,
                is_mut: false,
                is_reactive: false,
                type_id,
                skip_value_copy: false,
            },
            span,
        ));
        TirExpr::new(TirExprKind::Local { index, name }, type_id, span)
    }

    /// Reify a `MatchExpr`. The scrutinee is walked; each arm enters
    /// its own scope, reifies the pattern (which adds bindings to
    /// `ctx`), reifies the optional guard at `Bool`, and reifies the
    /// body at the match's `expected_type`. The result `TypeId` is
    /// the recorded type — annotate already unified arm body types
    /// into it.
    fn reify_match_expr(
        &mut self,
        match_expr: &ast::MatchExpr,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
        recorded_type: TypeId,
    ) -> TirExpr {
        use crate::tir::{TirExprKind, TirMatchArm, TypeTable};

        let scrutinee = self.reify_expr(&match_expr.expr, ctx, None);
        let scrutinee_type = scrutinee.type_id;

        // Same fallback as `reify_if_expr` (this file, above): without a
        // top-down expected type, an arm body that is a bare `null` reifies
        // as `UNKNOWN` unless it falls back to the match's own already-unified
        // `recorded_type`, producing the same invalid-Wasm shape for
        // `let x = match v { A => Option::Some(1), B => null };`.
        let branch_expected = expected_type.or(Some(recorded_type));

        let arms: Vec<TirMatchArm> = match_expr
            .arms
            .iter()
            .map(|arm| {
                ctx.enter_scope();
                let pattern = self.reify_pattern(&arm.pattern, scrutinee_type, ctx);
                let guard = arm
                    .guard
                    .as_ref()
                    .map(|g| self.reify_expr(g, ctx, Some(TypeTable::BOOL)));
                let body = self.reify_expr(&arm.body, ctx, branch_expected);
                ctx.exit_scope();
                TirMatchArm {
                    pattern,
                    guard,
                    body,
                    span: arm.span,
                }
            })
            .collect();

        TirExpr::new(
            TirExprKind::Match {
                expr: Box::new(scrutinee),
                arms,
            },
            recorded_type,
            match_expr.span,
        )
    }

    /// Reify a `StaticMethodCallExpr` — a fully-qualified static call like
    /// `Stream<u8>::new()`, whose target parses as a `Type` node rather than an
    /// `Ident` callee. Same shape as `reify_call`'s qualified-callee branch:
    /// resolve the target type, take the impl module from the resolved struct,
    /// and build the mangled `__Type__method` `FunctionRef`.
    fn reify_static_method_call(
        &mut self,
        static_call: &ast::StaticMethodCallExpr,
        ctx: &mut FunctionContext,
        recorded_type: TypeId,
    ) -> TirExpr {
        use crate::tir::{CallArg, ResolvedType, TirExprKind, TypeId};

        // Reuse the static-method `FunctionRef` annotate resolved
        // (mangled name + `cm_name` for CM binding synthesis). reify's
        // own target-type resolution can lose these for imported / CM
        // generic targets (`Future::<T>::new`, `Result::<…>::Ok`),
        // collapsing the struct name to empty and emitting an
        // unresolvable `::new` call. Variant-constructor turbofish shapes
        // are not recorded here (annotate returns before the static-call
        // path), so they fall through to the variant detection below.
        if let Some(dispatch) = self.ann_static_method_dispatch(static_call.id) {
            let mut args: Vec<CallArg> = static_call
                .args
                .iter()
                .zip(
                    dispatch
                        .param_is_mut
                        .iter()
                        .copied()
                        .chain(std::iter::repeat(false)),
                )
                .map(|(a, is_mut)| CallArg::new(self.reify_expr(a, ctx, None), is_mut))
                .collect();

            let callee_module = dispatch.function_ref.module_source.clone();
            self.reify_apply_param_defaults(
                &mut args,
                &dispatch.param_defaults,
                &dispatch.param_types,
                &callee_module,
                static_call.span,
                ctx,
            );

            // Replay the production `Call`'s exact type args (method-level;
            // impl args ride along in `function_ref.monomorph_info`).
            return TirExpr::new(
                TirExprKind::Call {
                    type_args: dispatch.type_args,
                    func: Box::new(dispatch.function_ref),
                    args,
                    has_receiver: false,
                },
                recorded_type,
                static_call.span,
            );
        }

        // Variant constructor in turbofish form (`Option::<T>::Some(x)`,
        // `Result::<T, E>::Ok(v)`): annotate's variant-ctor branch in
        // `resolve_static_method_call` types the expression as the variant
        // (line 1105+ / 1173+ in method_call.rs), so `recorded_type` is
        // always the variant instance and reify reads it directly.
        let variant_type = recorded_type;
        let variant_type_args: Vec<TypeId> =
            match self.tysys.type_table.borrow().get(recorded_type).clone() {
                ResolvedType::GenericInstance { type_args, .. } => type_args,
                _ => Vec::new(),
            };
        if let Some(variant_info) = self
            .tysys
            .type_def(recorded_type)
            .and_then(|def| self.type_lookup().variant_cases_of(def))
            .cloned()
            && let Some((case_index, case_data)) = variant_info
                .cases
                .iter()
                .enumerate()
                .find(|(_, c)| c.name == static_call.method)
                .map(|(i, c)| (i, c.clone()))
        {
            let payload_type = self.get_variant_case_payload_type(
                self.tysys.type_def(recorded_type),
                &static_call.method,
                &variant_type_args,
            );
            let payload = static_call
                .args
                .first()
                .map(|a| Box::new(self.reify_expr(a, ctx, Some(payload_type))));
            return TirExpr::new(
                TirExprKind::VariantConstruct {
                    variant_type,
                    case_index: case_index as u32,
                    case_name: case_data.name,
                    payload,
                },
                variant_type,
                static_call.span,
            );
        }

        // Everything else flows through `static_method_dispatch` above —
        // `resolve_static_method_call` records the resolved
        // `FunctionRef` for every non-variant static call. Hitting this
        // shape means annotate diagnosed an unresolvable call.
        TirExpr::new(
            TirExprKind::Unit,
            crate::tir::TypeTable::ERROR,
            static_call.span,
        )
    }

    /// Parameter `(name, default)` list of a free function in
    /// declaration order. Empty for unknown callees.
    fn lookup_free_func_params(
        &self,
        module_source: &ModuleSource,
        func_name: &str,
    ) -> Vec<(String, Option<ast::Expr>)> {
        let Some(idx_map) = self.tysys.loaded_module_func_indices.get(module_source) else {
            return Vec::new();
        };
        let Some(&idx) = idx_map.get(func_name) else {
            return Vec::new();
        };
        let items: &[Item] = if module_source == &self.current_module_source {
            self.current_module_items
        } else if let Some(m) = self.loaded_modules.get(module_source) {
            &m.items
        } else {
            return Vec::new();
        };
        if let Some(Item::Function(func)) = items.get(idx) {
            func.params
                .iter()
                .map(|p| (p.name.clone(), p.default.clone()))
                .collect()
        } else {
            Vec::new()
        }
    }

    /// Pad `args` with the defaults the call omitted, for either shape a
    /// dispatch names: a free function's parameter list, or a static method's
    /// own. Neither answers for the other, so both are asked.
    fn reify_pad_dispatch_defaults(
        &mut self,
        callee: &ast::Expr,
        args: &mut Vec<crate::tir::CallArg>,
        dispatch: &crate::elaborator::sem::types::StaticMethodDispatch,
        span: crate::token::Span,
        ctx: &mut FunctionContext,
    ) {
        self.reify_pad_args_with_defaults(
            callee,
            args,
            &dispatch.param_types,
            &dispatch.function_ref.module_source.clone(),
            &dispatch.function_ref.name.clone(),
            ctx,
        );
        let module = dispatch.defaults_module.clone();
        self.reify_apply_param_defaults(
            args,
            &dispatch.param_defaults,
            &dispatch.param_types,
            &module,
            span,
            ctx,
        );
    }

    fn reify_pad_args_with_defaults(
        &mut self,
        callee: &ast::Expr,
        args: &mut Vec<crate::tir::CallArg>,
        param_types: &[crate::tir::TypeId],
        callee_module: &ModuleSource,
        callee_name: &str,
        ctx: &mut FunctionContext,
    ) {
        // Only an ident callee names a free function with defaults. A
        // namespace-qualified callee (`g::make`) is still a free function —
        // `callee_module` / `callee_name` already carry its resolved home and
        // name — so do NOT bail on `::` here; `lookup_free_func_params` returns
        // empty for anything that is not a free function in `callee_module`.
        let ast::Expr::Ident(_) = callee else {
            return;
        };
        let func_params = self.lookup_free_func_params(callee_module, callee_name);
        self.reify_apply_param_defaults(
            args,
            &func_params,
            param_types,
            callee_module,
            callee.span(),
            ctx,
        );
    }

    /// Pad `args` with reified default values for the trailing `func_params`
    /// the call omitted. `func_params` is the callee's `(name, default)` list in
    /// declaration order, `callee_module` its defining module (for the
    /// perspective swap), and `call_span` the call site (for location literals).
    fn reify_apply_param_defaults(
        &mut self,
        args: &mut Vec<crate::tir::CallArg>,
        func_params: &[(String, Option<ast::Expr>)],
        param_types: &[crate::tir::TypeId],
        callee_module: &ModuleSource,
        call_span: crate::token::Span,
        ctx: &mut FunctionContext,
    ) {
        if func_params.is_empty() || args.len() >= func_params.len() {
            return;
        }
        // A default may reference an earlier parameter. The substituted
        // value is the caller's argument, already reified under the
        // caller's perspective in `args[i]` (and, for later defaults,
        // the synthesized value reified below). Map parameter name →
        // reified TIR so `reify_ident` returns it directly: re-resolving
        // the spliced caller AST under the callee's swapped perspective
        // (below) would key its AstIds against the wrong module's
        // annotations and mis-type the node. Save / restore so nested
        // defaults compose.
        let mut overrides: IndexMap<String, TirExpr> = IndexMap::default();
        for (i, arg) in args.iter().enumerate() {
            if let Some((name, _)) = func_params.get(i) {
                overrides.insert(name.clone(), arg.expr.clone());
            }
        }
        let saved_overrides = std::mem::replace(&mut self.default_arg_overrides, overrides);

        // Capture the call site for location literals before the perspective
        // swap below moves to the callee. Only the outermost default walk
        // captures; a nested defaulted call (`fn outer(x = loc())`) inherits
        // it, so every literal reports the same ultimate call site.
        let captured_call_site = self.call_site_location.is_none();
        if captured_call_site {
            self.call_site_location = Some(CallSiteLocation {
                module: self.current_module_source.clone(),
                span: call_span,
                function_name: ctx.function_name.clone(),
            });
        }

        // A default expression otherwise resolves in the *callee's* lexical
        // scope and may name items the caller cannot see, so swap the module
        // triple to the callee around the walk. The caller's `ctx` stays, so
        // earlier-param substitutions keep their bindings.
        let loaded = self.loaded_modules;
        let all_sem = self.all_module_semantics;
        let callee_ctx: Option<(&[Item], &ModuleSemantics)> =
            if callee_module == &self.current_module_source {
                None
            } else {
                match (loaded.get(callee_module), all_sem.get(callee_module)) {
                    (Some(m), Some(callee_sem)) => Some((m.items.as_slice(), callee_sem)),
                    _ => None,
                }
            };
        let saved = callee_ctx.map(|(items, callee_sem)| {
            (
                std::mem::replace(&mut self.current_module_source, callee_module.clone()),
                std::mem::replace(&mut self.current_module_items, items),
                std::mem::replace(&mut self.sem, callee_sem),
            )
        });

        for i in args.len()..func_params.len() {
            let (name, default_ast) = match func_params.get(i) {
                Some((n, Some(d))) => (n.clone(), d.clone()),
                _ => break,
            };
            // A default declared on a trait method has no body for annotate to
            // walk, so without the parameter's type here it reifies untyped.
            let expected = param_types.get(i).copied();
            let resolved = self.reify_expr(&default_ast, ctx, expected);
            // Later defaults may reference this one's parameter.
            self.default_arg_overrides.insert(name, resolved.clone());
            args.push(crate::tir::CallArg::new(resolved, false));
        }

        if let Some((src, items, sem)) = saved {
            self.current_module_source = src;
            self.current_module_items = items;
            self.sem = sem;
        }
        self.default_arg_overrides = saved_overrides;
        if captured_call_site {
            self.call_site_location = None;
        }
    }

    /// A function-typed global's `GlobalVarGet` parts
    /// `(module_source, global_name, type)`, or `None` for a non-global or
    /// non-function name. Shares `ModuleDecls::lookup_global` with the
    /// annotate-side `Elaborator::global_var_type` so the two paths agree.
    fn global_fn_callee(&self, name: &str) -> Option<(ModuleSource, String, TypeId)> {
        let (module_source, global_name, ty, _mutable) = self
            .sem
            .decls
            .lookup_global(name, &self.current_module_source)?;
        let table = self.tysys.type_table.borrow();
        let base = table.representation_head(table.peel_refs(ty));
        matches!(table.get(base), crate::tir::ResolvedType::Function { .. }).then_some((
            module_source,
            global_name,
            ty,
        ))
    }

    /// Reify a `CallExpr`, mirroring `Elaborator::resolve_call`
    /// The arms below are ordered by precedence and each
    /// documents the recorded fact it reads; nothing here re-resolves a
    /// callee.
    fn reify_call(
        &mut self,
        call: &ast::CallExpr,
        ctx: &mut FunctionContext,
        recorded_type: TypeId,
    ) -> TirExpr {
        use crate::tir::{CallArg, TirExprKind};

        let span = call.span;

        // Variant-ctor (`Variant::Case(payload)`) must beat the
        // `static_method_dispatch` arm: annotate also records the
        // ctor there too, but that shape would lower to a
        // `Call` against a function that doesn't exist.
        if let ast::Expr::Ident(ident) = &call.callee
            && let Some((owner, spelled)) = self.case_path(ident)
            && let Some((prefix, suffix)) = spelled.split_once("::")
        {
            if !suffix.contains("::") {
                let lookup = self.type_lookup();
                if let Some(variant_info) = owner
                    .and_then(|owner| lookup.variant_cases_of(owner))
                    .cloned()
                    && let Some((case_index, case_data)) = variant_info
                        .cases
                        .iter()
                        .enumerate()
                        .find(|(_, c)| c.name == suffix)
                        .map(|(i, c)| (i, c.clone()))
                {
                    let variant_type = self
                        .ann_generic_instantiations(call.id)
                        .map(|gi| gi.instance_type)
                        .unwrap_or(recorded_type);
                    let payload = call.args.first().map(|arg_expr| {
                        Box::new(self.reify_expr(arg_expr, ctx, Some(case_data.payload)))
                    });
                    return TirExpr::new(
                        TirExprKind::VariantConstruct {
                            variant_type,
                            case_index: case_index as u32,
                            case_name: case_data.name,
                            payload,
                        },
                        variant_type,
                        span,
                    );
                }
            } else if let Some(inner) = suffix.find("::")
                && let Some(ns_source) = self.sem.imports.namespace_imports.get(prefix).cloned()
            {
                // `ns::Type::Case(payload)` — a namespace-imported variant
                // constructor with a payload. The nullary form is handled
                // in `reify_ident`; the payload form parses as a `Call`.
                // The case lives in the namespace's variant table; the
                // instance type is the call's recorded expression type
                // (annotate resolved it).
                let type_name = &suffix[..inner];
                let case_name = &suffix[inner + 2..];
                if let Some(variant_info) = self
                    .qualified_owner_decl(ident)
                    .and_then(|def| self.tysys.all_variant_cases.get(&def))
                    .cloned()
                    && let Some((case_index, case_data)) = variant_info
                        .cases
                        .iter()
                        .enumerate()
                        .find(|(_, c)| c.name == case_name)
                        .map(|(i, c)| (i, c.clone()))
                {
                    let variant_type = self
                        .ann_generic_instantiations(call.id)
                        .map(|gi| gi.instance_type)
                        .unwrap_or(recorded_type);
                    let payload = call.args.first().map(|arg_expr| {
                        Box::new(self.reify_expr(arg_expr, ctx, Some(case_data.payload)))
                    });
                    return TirExpr::new(
                        TirExprKind::VariantConstruct {
                            variant_type,
                            case_index: case_index as u32,
                            case_name: case_data.name,
                            payload,
                        },
                        variant_type,
                        span,
                    );
                }

                // `ns::Type::method(args)` is handled by the
                // `static_method_dispatch` early return below — annotate
                // now records the resolved `FunctionRef` for namespace-
                // qualified static calls too (see call.rs's ns-static
                // branch). The fallthrough here is intentional.
                let _ = (ns_source, type_name, case_name);
            }
        }

        // Static-method / builtin dispatch (`Type::method(args)`,
        // `builtin::fn(args)`): the elaborator recorded the resolved
        // `FunctionRef` on `sem.types.static_method_dispatch` so reify
        // can reproduce the same TIR `Call` shape without re-running
        // impl lookup, mangled-name construction, or monomorph-info
        // shaping (none of which are tractable from the AST alone).
        if let Some(dispatch) = self.ann_static_method_dispatch(call.id) {
            // Forward per-argument expected types to closure args with an
            // unannotated param, or their params stay UNKNOWN and the functor's
            // `__call` is dropped before codegen. Restricted to those closures
            // so an effect-polymorphic one keeps its body-inferred effects
            // instead of being pinned to a generic effect param.
            let call_param_types = self.ann_call_param_types(call.id);
            let mut arg_exprs: Vec<CallArg> = call
                .args
                .iter()
                .enumerate()
                .zip(
                    dispatch
                        .param_is_mut
                        .iter()
                        .copied()
                        .chain(std::iter::repeat(false)),
                )
                .map(|((i, a), is_mut)| {
                    let expected = if arg_is_unannotated_closure(a) {
                        call_param_types
                            .as_ref()
                            .and_then(|pts| pts.get(i).copied())
                    } else {
                        None
                    };
                    let arg = self.reify_expr(a, ctx, expected);
                    CallArg::new(arg, is_mut)
                })
                .collect();
            self.reify_pad_dispatch_defaults(&call.callee, &mut arg_exprs, &dispatch, span, ctx);
            // Type args: replay exactly what the production builder put on
            // the `Call`. This already folds in any explicit turbofish and,
            // crucially, carries only the method-level type args — a generic
            // struct's impl type args live in `function_ref.monomorph_info`,
            // so re-deriving from `generic_instantiations` (which is the flat
            // impl+method list) would mangle `Container<i32>::make` as
            // `Container::make<i32>` and miss the monomorphized instance.
            return TirExpr::new(
                TirExprKind::Call {
                    type_args: dispatch.type_args,
                    func: Box::new(dispatch.function_ref),
                    args: arg_exprs,
                    has_receiver: false,
                },
                recorded_type,
                span,
            );
        }

        // `Type::from(x)` with no explicit `From` impl — reflexive and
        // newtype conversions. Production's `resolve_call` handles these
        // inline and records no `static_method_dispatch`,
        // tagging the reflexive case with `NewtypeFromCollapse`; reify must
        // reproduce the same three shapes (otherwise it falls through to an
        // unresolvable `Type::from` `Call`). Only reached when a user `From`
        // impl coexists, since that routes `from` through the static-call
        // path while the builtin reflexive/newtype conversion stays implicit.
        if let ast::Expr::Ident(ident) = &call.callee
            && let Some(pos) = ident.name.find("::")
            && &ident.name[pos + 2..] == "from"
            && !ident.name[pos + 2..].contains("::")
            && call.args.len() == 1
        {
            let arg = self.reify_expr(&call.args[0], ctx, None);
            let arg_type = arg.type_id;

            // Bodyless `impl From<X> for Type;` marker impl — production
            // synthesizes a `From::from` call inline via
            // `resolve_from_call` and records `FromCallFacts`
            // under `call.id`. Reify reuses `reify_from_call` so both the
            // ?-op path and this static-call path emit identical TIR.
            if self.ann_from_call_facts(call.id).is_some() {
                return self.reify_from_call(recorded_type, arg_type, arg, span, call.id);
            }

            // Reflexive: `T::from(T_val)` — identity, return the argument.
            // Annotate tags the call with `NewtypeFromCollapse`; reify
            // recognises it and emits the argument's TIR directly.
            if self.ann_desugars(call.id)
                == Some(super::sem::types::DesugarKind::NewtypeFromCollapse)
            {
                return arg;
            }

            // Newtype→Base: `Base::from(Newtype_val)`. Annotate records
            // `NewtypeFromUnwrap` on the call and lowers to a `Cast` to
            // the base type; reify replays the shape using the recorded
            // expression type (which is the base type).
            if self.ann_desugars(call.id) == Some(super::sem::types::DesugarKind::NewtypeFromUnwrap)
            {
                return TirExpr::new(
                    TirExprKind::Cast {
                        expr: Box::new(arg),
                        target_type: recorded_type,
                    },
                    recorded_type,
                    span,
                );
            }

            // Base→Newtype: `Newtype::from(Base_val)`. Annotate records
            // `NewtypeFromWrap` on the call and lowers to a `Cast` to the
            // newtype; reify replays the shape using the recorded
            // expression type (which is the newtype).
            if self.ann_desugars(call.id) == Some(super::sem::types::DesugarKind::NewtypeFromWrap) {
                return TirExpr::new(
                    TirExprKind::Cast {
                        expr: Box::new(arg),
                        target_type: recorded_type,
                    },
                    recorded_type,
                    span,
                );
            }

            // Not a reflexive/newtype `from` — fall through to the generic
            // call handling below, which reifies args itself; `arg` here is
            // dropped (no side effects: `reify_expr` is pure TIR shaping).
        }

        // Closure-call shape: bare-ident callee that resolves to a
        // local with `fn(...)` type. Annotate decides this by
        // probing `ctx.lookup`; reify reproduces by checking the
        // ident's local + its resolved type. The same `ctx` reify
        // built during the body walk has every let-bound local in
        // place (the walk-order invariant), so the lookup returns the
        // same answer.
        if let ast::Expr::Ident(ident) = &call.callee
            && !ident.name.contains("::")
            && let Some(local) = ctx.lookup(&ident.name)
            && {
                // The callee may be a bare `fn(...)` value or a reference
                // to one (`&fn(...)`, `&mut fn(...)`), possibly behind a
                // fn-type newtype. Mirror `Elaborator::as_fn_signature`:
                // peel references and the ultimate base type before
                // checking for `Function`.
                let table = self.tysys.type_table.borrow();
                let base = table.representation_head(table.peel_refs(local.type_id));
                matches!(table.get(base), crate::tir::ResolvedType::Function { .. })
            }
        {
            let local_index = local.index;
            let local_type_id = local.type_id;
            let callee_expr = TirExpr::new(
                TirExprKind::Local {
                    index: local_index,
                    name: ident.name.clone(),
                },
                local_type_id,
                ident.span,
            );
            // Auto-deref a `&fn` / `&mut fn` callee down to the function
            // value, exactly as `build_indirect_call`'s final
            // `deref_to_value` does in the production path.
            let callee_expr = deref_to_value(callee_expr, ident.span, &self.tysys.type_table);
            let arg_exprs: Vec<TirExpr> = call
                .args
                .iter()
                .map(|a| self.reify_expr(a, ctx, None))
                .collect();
            return TirExpr::new(
                TirExprKind::IndirectCall {
                    callee: Box::new(callee_expr),
                    args: arg_exprs,
                },
                recorded_type,
                span,
            );
        }

        // Global closure call: a bare-ident callee that is not a local but
        // names a *global* (current-module or imported) of `fn(...)` type.
        // Mirrors `resolve_call`'s global path. Annotate records no type for
        // the callee (like the local-variable path), so build the global read
        // directly with the global's type rather than via `reify_expr`.
        if let ast::Expr::Ident(ident) = &call.callee
            && !ident.name.contains("::")
            && ctx.lookup(&ident.name).is_none()
            && let Some((module_source, global_name, callee_ty)) =
                self.global_fn_callee(&ident.name)
        {
            let callee_expr = TirExpr::new(
                TirExprKind::GlobalVarGet {
                    module_source,
                    name: global_name,
                },
                callee_ty,
                ident.span,
            );
            let callee_expr = deref_to_value(callee_expr, ident.span, &self.tysys.type_table);
            let arg_exprs: Vec<TirExpr> = call
                .args
                .iter()
                .map(|a| self.reify_expr(a, ctx, None))
                .collect();
            return TirExpr::new(
                TirExprKind::IndirectCall {
                    callee: Box::new(callee_expr),
                    args: arg_exprs,
                },
                recorded_type,
                span,
            );
        }

        // A bare-ident callee that names a value binding (local/param or
        // global) which is *not* function-typed — the fn cases returned
        // above. Annotate already emitted `CalleeNotCallable`; recover with an
        // error node so the free-function lookup below does not pile a second
        // ("unknown function") diagnostic on top.
        if let ast::Expr::Ident(ident) = &call.callee
            && !ident.name.contains("::")
            && (ctx.lookup(&ident.name).is_some()
                || self
                    .sem
                    .decls
                    .lookup_global(&ident.name, &self.current_module_source)
                    .is_some())
        {
            return TirExpr::new(TirExprKind::Unit, crate::tir::TypeTable::ERROR, span);
        }

        // Indirect-call shape: callee is any non-ident expression
        // whose type resolves to a function (e.g. `arr[i](x)`,
        // `(foo.bar)(x)`, `(get_fn())(x)`, `(|x| x)(1)`). Mirrors
        // `Elaborator::resolve_call`'s non-ident-callee path
        if !matches!(&call.callee, ast::Expr::Ident(_)) {
            let callee_expr = self.reify_expr(&call.callee, ctx, None);
            let is_fn = {
                let table = self.tysys.type_table.borrow();
                let base = table.representation_head(table.peel_refs(callee_expr.type_id));
                matches!(table.get(base), crate::tir::ResolvedType::Function { .. })
            };
            if is_fn {
                // Auto-deref a `&fn` / `&mut fn` callee, matching
                // `build_indirect_call`'s `deref_to_value` in production.
                let callee_expr =
                    deref_to_value(callee_expr, call.callee.span(), &self.tysys.type_table);
                let arg_exprs: Vec<TirExpr> = call
                    .args
                    .iter()
                    .map(|a| self.reify_expr(a, ctx, None))
                    .collect();
                return TirExpr::new(
                    TirExprKind::IndirectCall {
                        callee: Box::new(callee_expr),
                        args: arg_exprs,
                    },
                    recorded_type,
                    span,
                );
            }
            // Non-fn-typed non-ident callee — annotate already
            // diagnosed it (`TypeError::CalleeNotCallable`).
            // Match the elaborator's recovery shape.
            return TirExpr::new(TirExprKind::Unit, crate::tir::TypeTable::ERROR, span);
        }

        // Free-function call: bare-ident callee that names a current-
        // module or imported function.
        if let ast::Expr::Ident(ident) = &call.callee
            && !ident.name.contains("::")
        {
            let (callee_module, callee_name) = if self
                .sem
                .decls
                .function_return_types
                .contains_key(&ident.name)
            {
                (self.current_module_source.clone(), ident.name.clone())
            } else if let Some(def) = self.tysys.resolutions.declared_if_walked(ident.id) {
                let defs = self.tysys.resolutions.defs();
                (defs.module(def).clone(), defs.name(def).to_string())
            } else {
                // The callee resolves neither as a local fn-typed
                // value (closure-call branch above) nor as a known
                // free / imported function. The remaining shapes
                // are namespaced calls (`ns::foo(x)`, with `ns`
                // resolved via `sem.imports.namespace_imports`).
                // Annotate has diagnosed truly-unresolved names.
                if let Some(double_colon) = ident.name.find("::") {
                    let ns_prefix = &ident.name[..double_colon];
                    let rest = &ident.name[double_colon + 2..];
                    if let Some(ns_source) =
                        self.sem.imports.namespace_imports.get(ns_prefix).cloned()
                        && !rest.contains("::")
                    {
                        // `resolve_call`'s free-function path records the
                        // final `type_args` (turbofish + inferred) on
                        // `generic_instantiations`; reify reads it.
                        let type_args: Vec<TypeId> = self
                            .ann_generic_instantiations(call.id)
                            .map(|gi| gi.type_args)
                            .unwrap_or_default();
                        let arg_calls: Vec<CallArg> = call
                            .args
                            .iter()
                            .map(|a| CallArg::new(self.reify_expr(a, ctx, None), false))
                            .collect();
                        return TirExpr::new(
                            TirExprKind::Call {
                                func: Box::new(crate::tir::FunctionRef {
                                    module_source: ns_source,
                                    name: rest.to_string(),
                                    monomorph_info: None,
                                    method_info: None,
                                }),
                                type_args,
                                args: arg_calls,
                                has_receiver: false,
                            },
                            recorded_type,
                            span,
                        );
                    }
                }
                // Unresolved: emit recovery shape matching
                // annotate's diagnostic path.
                return TirExpr::new(TirExprKind::Unit, crate::tir::TypeTable::ERROR, span);
            };

            // Type args: `resolve_call` records the final `type_args`
            // (turbofish + inferred) on `generic_instantiations`; reify
            // reads it. Non-generic calls leave no entry and reify gets
            // the empty vector, matching the production builder.
            let type_args: Vec<TypeId> = self
                .ann_generic_instantiations(call.id)
                .map(|gi| gi.type_args)
                .unwrap_or_default();

            // Per-argument expected types come from the recorded resolved
            // param types. They are required for unannotated-param closure
            // args (`|a, b| ...`) coerced to a `fn`-typed (or `fn`-newtype)
            // param, so the closure infers its params and produces the functor
            // specialization the call site needs. Literal re-coercion and
            // `is_mut` per-arg are still handled elsewhere (`coercions`); see
            // `arg_is_unannotated_closure` for why the forward is restricted.
            let call_param_types = self.ann_call_param_types(call.id);
            let mut args: Vec<CallArg> = call
                .args
                .iter()
                .enumerate()
                .map(|(i, a)| {
                    let expected = if arg_is_unannotated_closure(a) {
                        call_param_types
                            .as_ref()
                            .and_then(|pts| pts.get(i).copied())
                    } else {
                        None
                    };
                    let arg = self.reify_expr(a, ctx, expected);
                    CallArg::new(arg, false)
                })
                .collect();

            // Pad omitted trailing args with the callee's declared defaults,
            // matching annotate's `pad_args_with_defaults` (call.rs). Without
            // this the type checker sees the padded arity but the TIR keeps
            // only the explicit args, so WIR lowers a call with a missing
            // trailing operand.
            let param_types = self.ann_call_param_types(call.id).unwrap_or_default();
            self.reify_pad_args_with_defaults(
                &call.callee,
                &mut args,
                &param_types,
                &callee_module,
                &callee_name,
                ctx,
            );

            return TirExpr::new(
                TirExprKind::Call {
                    func: Box::new(crate::tir::FunctionRef {
                        module_source: callee_module,
                        name: callee_name,
                        monomorph_info: None,
                        method_info: None,
                    }),
                    type_args,
                    args,
                    has_receiver: false,
                },
                recorded_type,
                span,
            );
        }

        // Qualified-callee `Type::method` shapes that don't flow through
        // `static_method_dispatch` recording: `Flags::none()` /
        // `Flags::all()` lower to an `IntLiteral` (not a `Call`) so
        // the body walk's static-method recording in
        // `resolve_call` skips them. Reify reproduces the same
        // `IntLiteral` here.
        if let ast::Expr::Ident(ident) = &call.callee
            && let Some(pos) = ident.name.find("::")
            && !ident.name[pos + 2..].contains("::")
        {
            let prefix = &ident.name[..pos];
            let suffix = &ident.name[pos + 2..];

            let owner = self.qualified_owner_site(ident);
            // A newtype reaches its base's constants and keeps its own type.
            let through_newtype = self.newtype_member_owner(owner, prefix);
            let flags = match through_newtype {
                Some((base, _)) => self.type_lookup().flags_members_of(base).cloned(),
                None => self.type_lookup().flags_members_at(owner, prefix).cloned(),
            };
            if let Some(flags_info) = flags
                && matches!(suffix, "none" | "all")
            {
                let member_count = flags_info.members.len() as u32;
                let value: u64 = match suffix {
                    "none" => 0,
                    "all" => u64::from((1u32 << member_count) - 1),
                    _ => unreachable!(),
                };
                return TirExpr::new(
                    TirExprKind::IntLiteral {
                        value,
                        repr: value.to_string(),
                    },
                    through_newtype.map_or(flags_info.type_id, |(_, named)| named),
                    span,
                );
            }
        }

        // Unrecognised callee shape — annotate diagnosed it.
        TirExpr::new(TirExprKind::Unit, crate::tir::TypeTable::ERROR, span)
    }

    /// Reify the `container[i].method(args)` `IndexMut` rewrite from two
    /// dispatch records: `operator_dispatch[index_expr.id]` for the inner
    /// `index_mut(idx)` and `method_dispatch[method_call.id]` for the outer
    /// call. Builds `container.index_mut(idx)`, then adjusts the receiver by
    /// the outer dispatch's `self_kind` / `is_ref_impl`.
    fn reify_index_mut_method_call(
        &mut self,
        method_call: &ast::MethodCallExpr,
        ctx: &mut FunctionContext,
        recorded_type: TypeId,
    ) -> TirExpr {
        use crate::tir::CallArg;

        // The AST receiver of the IndexMutMethodCall is always an
        // `Expr::Index` — guaranteed by the elaborator's
        // dispatcher; reify trusts the desugar tag's contract.
        let ast::Expr::Index(index_expr) = &method_call.receiver else {
            panic!(
                "reify_index_mut_method_call: receiver is not an IndexExpr (desugar invariant violated)"
            );
        };

        let inner_dispatch = self
            .ann_operator_dispatch(index_expr.id)
            .expect(
                "reify_index_mut_method_call: inner IndexMut dispatch missing — annotate should have recorded it alongside the IndexMutMethodCall desugar tag",
            );

        let outer_dispatch = self
            .ann_method_dispatch(method_call.id)
            .expect(
                "reify_index_mut_method_call: outer method dispatch missing — annotate should have recorded it via record_method_dispatch",
            );

        // Step 1: build the `container.index_mut(idx)` call.
        let container = self.reify_expr(&index_expr.expr, ctx, None);
        let receiver_for_index_mut = adjust_receiver_for_self_kind(
            container,
            inner_dispatch.self_kind,
            false,
            index_expr.span,
            &self.tysys.type_table,
        );
        let index_resolved = self.reify_expr(&index_expr.index, ctx, None);
        let index_mut_call = build_tir_method_call(
            receiver_for_index_mut,
            inner_dispatch.function_ref,
            vec![],
            vec![CallArg::new(index_resolved, false)],
            inner_dispatch.return_type,
            index_expr.span,
        );

        // Step 2: adjust the index_mut result for the outer method's
        // self_kind and build the outer method-call TIR.
        let receiver_for_method = adjust_receiver_for_self_kind(
            index_mut_call,
            outer_dispatch.self_kind,
            outer_dispatch.is_ref_impl,
            method_call.span,
            &self.tysys.type_table,
        );

        // Method-level type args ride along on `MethodDispatch` — the
        // IndexMut rewrite in `method_lookup.rs` records the same vector
        // it passes to `build_tir_method_call`, so reify is a pure read.
        let type_args = outer_dispatch.method_type_args.clone();
        let args: Vec<CallArg> = method_call
            .args
            .iter()
            .map(|a| CallArg::new(self.reify_expr(a, ctx, None), false))
            .collect();

        let result_type = if outer_dispatch.return_type == crate::tir::TypeTable::UNKNOWN {
            recorded_type
        } else {
            outer_dispatch.return_type
        };
        build_tir_method_call(
            receiver_for_method,
            outer_dispatch.function_ref,
            type_args,
            args,
            result_type,
            method_call.span,
        )
    }

    /// Reify a `MethodCallExpr`. Every decision — resolved `FunctionRef`,
    /// receiver-adjustment kind, ref-impl flag, final type — is already on
    /// `sem.types`. An `IndexMutMethodCall` desugar routes through here too,
    /// materialising `__index_mut_val` before the dispatch.
    fn reify_method_call(
        &mut self,
        method_call: &ast::MethodCallExpr,
        ctx: &mut FunctionContext,
        recorded_type: TypeId,
    ) -> TirExpr {
        use crate::tir::{TirExprKind, TypeTable};

        // IndexMut rewrite gets first crack — when the elaborator
        // tagged this call as `IndexMutMethodCall`, the receiver is an
        // index expression that needs `__index_mut_val` synthesis.
        if matches!(
            self.ann_desugars(method_call.id),
            Some(super::sem::types::DesugarKind::IndexMutMethodCall)
        ) {
            return self.reify_index_mut_method_call(method_call, ctx, recorded_type);
        }

        // Synthetic-call shortcuts: `tuple.len()` / `tuple.zip()`
        // bypass method dispatch entirely (the elaborator's
        // `resolve_method_call_with` short-circuits at the
        // receiver-type check, leaving no `method_dispatch` entry).
        // Reify recognises tuple-typed receivers and emits the
        // direct TIR shape. See WEP §"Synthetic call sites stay
        // annotation-free by design".
        if matches!(method_call.method.as_str(), "len" | "zip") {
            let receiver = self.reify_expr(&method_call.receiver, ctx, None);
            // Auto-deref through `&`/`&mut` so tuple `.len()` / `.zip()` work
            // on a reference receiver, like any other method (mirrors the
            // elaborator's `get_base_type`). The `receiver` expr is kept as-is
            // for field access, which auto-derefs.
            let base_type_id = self.tysys.type_table.borrow().peel_refs(receiver.type_id);
            let is_tuple_receiver = self.tysys.type_table.borrow().is_tuple(base_type_id);
            if is_tuple_receiver {
                return match method_call.method.as_str() {
                    "len" => {
                        // A tuple type still carrying a `..T` pack has an arity
                        // unknown until monomorphization; defer folding to a
                        // literal via `TupleLen` so it is not frozen at the
                        // unsubstituted pack count (mirrors the `zip` deferral).
                        if self.type_contains_pack(base_type_id) {
                            return TirExpr::new(
                                TirExprKind::TupleLen {
                                    expr: Box::new(receiver),
                                },
                                TypeTable::I32,
                                method_call.span,
                            );
                        }
                        let len = self
                            .tysys
                            .type_table
                            .borrow()
                            .as_tuple(base_type_id)
                            .map(|elems| elems.len())
                            .unwrap_or(0) as i64;
                        TirExpr::new(
                            TirExprKind::IntLiteral {
                                value: len as u64,
                                repr: len.to_string(),
                            },
                            TypeTable::I32,
                            method_call.span,
                        )
                    }
                    "zip" => {
                        // A concrete tuple-of-tuples transposes inline here;
                        // only a type-pack receiver defers expansion to the
                        // monomorphiser via `TupleZip`. Non-generic bodies
                        // never reach the monomorphiser, so emitting
                        // `TupleZip` here would hit `lower::translate`'s
                        // `unreachable!`.
                        if self.type_contains_pack(base_type_id) {
                            TirExpr::new(
                                TirExprKind::TupleZip {
                                    expr: Box::new(receiver),
                                },
                                recorded_type,
                                method_call.span,
                            )
                        } else {
                            // [[A0, A1], [B0, B1]].zip() → [[A0, B0], [A1, B1]]
                            let outer_elems = self
                                .tysys
                                .type_table
                                .borrow()
                                .as_tuple(base_type_id)
                                .unwrap();
                            let inner_arities: Vec<Vec<TypeId>> = outer_elems
                                .iter()
                                .map(|e| self.tysys.type_table.borrow().as_tuple(*e).unwrap())
                                .collect();
                            let arity = inner_arities[0].len();
                            let num_rows = outer_elems.len();
                            let mut col_exprs = Vec::with_capacity(arity);
                            for col in 0..arity {
                                let mut row_exprs = Vec::with_capacity(num_rows);
                                for (row, row_types) in inner_arities.iter().enumerate() {
                                    let row_access = TirExpr::new(
                                        TirExprKind::FieldAccess {
                                            expr: Box::new(receiver.clone()),
                                            field_index: row as u32,
                                            field_name: row.to_string(),
                                        },
                                        outer_elems[row],
                                        method_call.span,
                                    );
                                    let cell = TirExpr::new(
                                        TirExprKind::FieldAccess {
                                            expr: Box::new(row_access),
                                            field_index: col as u32,
                                            field_name: col.to_string(),
                                        },
                                        row_types[col],
                                        method_call.span,
                                    );
                                    row_exprs.push(cell);
                                }
                                let col_types: Vec<TypeId> =
                                    inner_arities.iter().map(|row| row[col]).collect();
                                let col_tuple_type =
                                    self.tysys.type_table.borrow_mut().make_tuple(col_types);
                                col_exprs.push(TirExpr::new(
                                    TirExprKind::TupleLiteral {
                                        elements: row_exprs,
                                    },
                                    col_tuple_type,
                                    method_call.span,
                                ));
                            }
                            TirExpr::new(
                                TirExprKind::TupleLiteral {
                                    elements: col_exprs,
                                },
                                recorded_type,
                                method_call.span,
                            )
                        }
                    }
                    _ => unreachable!(),
                };
            }
        }

        let dispatch = self.ann_method_dispatch(method_call.id).unwrap_or_else(|| {
            // Method lookup failed during annotate (error-recovery
            // path). Reify produces a placeholder `Unit` of `ERROR`
            // type so downstream phases see the same shape annotate
            // would have built; the actual diagnostic was already
            // emitted by the elaborator.
            panic!(
                "reify_method_call: dispatch annotation missing for `{}` — \
                     annotate should have recorded or short-circuited via desugar",
                method_call.method
            )
        });

        // Reify receiver and adjust per the dispatch contract, sharing the
        // adjuster with the elaborator so the same TIR shape
        // (Unary{Ref}/Unary{MutRef}/Deref wrapping) lands.
        let raw_receiver = self.reify_expr(&method_call.receiver, ctx, None);

        // Track implicit `&mut self` borrowing for primitive / enum local
        // receivers, mirroring `Elaborator::resolve_method_call_with`
        // a scalar-backed value is copied by default,
        // so `x.bump()` must mark `x` address-taken or the boxing pass won't
        // write the mutation back. Enums are plain discriminants — the same
        // scalar shape as primitives.
        let needs_implicit_mut_borrow =
            !dispatch.is_ref_impl && matches!(dispatch.self_kind, ast::SelfKind::MutRef) && {
                let tt = self.tysys.type_table.borrow();
                !matches!(
                    tt.get(raw_receiver.type_id),
                    crate::tir::ResolvedType::Ref(_) | crate::tir::ResolvedType::MutRef(_)
                ) && matches!(
                    tt.get(tt.representation_head(raw_receiver.type_id)),
                    crate::tir::ResolvedType::Primitive(_) | crate::tir::ResolvedType::Enum { .. }
                )
            };
        if needs_implicit_mut_borrow && let TirExprKind::Local { index, .. } = &raw_receiver.kind {
            ctx.address_taken_locals.insert(*index);
        }

        let adjusted_receiver = adjust_receiver_for_self_kind(
            raw_receiver,
            dispatch.self_kind,
            dispatch.is_ref_impl,
            method_call.span,
            &self.tysys.type_table,
        );

        // Method-level type args for the TIR method-call node — the exact
        // vector annotate fed into `build_tir_method_call`. The monomorphizer's
        // `collect_func_instantiation_sites` keys off this field to queue
        // `Struct^Trait::method<Args>` instances, so it must be exactly what
        // annotate resolved (turbofish-resolved or inference-recovered).
        // Reading it from `MethodDispatch` keeps the
        // blanket-impl turbofish case correct (where
        // `monomorph_info.method_type_args` is zeroed by design).
        let type_args = dispatch.method_type_args.clone();

        // Per-arg `is_mut` comes from the recorded `MethodDispatch`, off the
        // signature of the method annotate dispatched to.
        // Zip with the AST args so call sites with fewer args than
        // declared (a Stage-5 recovery shape) still produce the
        // right is_mut for the args we have.
        let mut args: Vec<crate::tir::CallArg> = method_call
            .args
            .iter()
            .zip(
                dispatch
                    .param_is_mut
                    .iter()
                    .copied()
                    .chain(std::iter::repeat(false)),
            )
            .map(|(a, is_mut)| {
                let arg_tir = self.reify_expr(a, ctx, None);
                crate::tir::CallArg::new(arg_tir, is_mut)
            })
            .collect();

        // Pad missing trailing args with the method's defaults.
        // Mirrors `resolve_method_call_with`; the recorded `param_names` /
        // `param_defaults` arrive on `MethodDispatch` from annotate.
        if args.len() < dispatch.param_defaults.len() {
            let mut subs: IndexMap<String, ast::Expr> = IndexMap::default();
            for (i, arg_ast) in method_call.args.iter().enumerate() {
                if let Some(name) = dispatch.param_names.get(i) {
                    subs.insert(name.clone(), arg_ast.clone());
                }
            }
            for i in args.len()..dispatch.param_defaults.len() {
                let Some(Some(default_ast)) = dispatch.param_defaults.get(i) else {
                    break;
                };
                let mut default_expr = default_ast.clone();
                default_expr.substitute_idents(&subs);
                let resolved = self.reify_expr(&default_expr, ctx, None);
                let is_mut = dispatch.param_is_mut.get(i).copied().unwrap_or(false);
                args.push(crate::tir::CallArg::new(resolved, is_mut));
                if let Some(name) = dispatch.param_names.get(i) {
                    subs.insert(name.clone(), default_expr);
                }
            }
        }

        // The call's result type is the resolved method's return type
        // (recorded on the dispatch), not the per-`AstId` `expression_types`
        // entry: that entry can carry a wrong type for the call site, which
        // would make a unit-returning call look value-producing and emit a
        // spurious `drop` of a value-less call (Wasm stack underflow). Fall
        // back to `recorded_type` only if the dispatch somehow lacks it.
        let result_type = if dispatch.return_type == TypeTable::UNKNOWN {
            recorded_type
        } else {
            dispatch.return_type
        };
        build_tir_method_call(
            adjusted_receiver,
            dispatch.function_ref,
            type_args,
            args,
            result_type,
            method_call.span,
        )
    }

    /// Resolve a field access to `(index, canonical_name, field_type)` against
    /// the receiver's struct decl. The field type is generic-substituted with
    /// the receiver's `type_args` and is the authoritative source for the
    /// access's `TirExpr::type_id` — unlike `expression_types[field.id]`, which
    /// collides across template sub-parsers. `None` means an unknown receiver.
    fn lookup_struct_field_index(
        &self,
        receiver_type: TypeId,
        field_name: &str,
    ) -> (u32, String, Option<TypeId>) {
        use crate::tir::ResolvedType;
        let resolved = self.tysys.type_table.borrow().get(receiver_type).clone();
        // The receiver's own head answers: same-named structs in different
        // modules reach their own fields, and an anonymous shape — which no
        // spelling names — reaches its own.
        let (head, type_args): (Option<crate::tir::StructDef>, Vec<TypeId>) = match resolved {
            ResolvedType::Struct { def, .. } => (Some(def), vec![]),
            ResolvedType::GenericInstance { type_args, .. } => {
                // Tuple projection (`t.0`): a tuple has no struct decl, so the
                // struct-fields lookup below misses and the `(0, …)` fallback
                // would collapse every `t.N` onto field 0. Resolve the numeric
                // field name into the element index directly.
                let name = self
                    .tysys
                    .type_table
                    .borrow()
                    .nominal_head(receiver_type)
                    .map(|(n, _)| n)
                    .unwrap_or_default();
                if crate::tir::TypeTable::is_tuple_type(&name)
                    && let Ok(index) = field_name.parse::<usize>()
                    && let Ok(elem) = super::Elaborator::<H>::tuple_literal_index_type(
                        &self.tysys.type_table,
                        &type_args,
                        index,
                    )
                {
                    return (index as u32, field_name.to_string(), Some(elem));
                }
                (
                    self.tysys
                        .type_def(receiver_type)
                        .map(crate::tir::StructDef::Decl),
                    type_args,
                )
            }
            // Peel references and newtypes and recurse, mirroring the
            // elaborator's `lookup_field_type`: `&Point`, `&mut Point`, a
            // newtype `Location = Point`, and chained newtypes / `&Location`
            // all resolve their fields against the ultimate underlying struct.
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
                return self.lookup_struct_field_index(inner, field_name);
            }
            ResolvedType::Newtype { base_type, .. } => {
                return self.lookup_struct_field_index(base_type, field_name);
            }
            _ => return (0, field_name.to_string(), None),
        };

        let found = head.and_then(|head| {
            self.type_lookup()
                .struct_fields_of_head(head)
                .and_then(|info| {
                    info.fields
                        .iter()
                        .enumerate()
                        .find(|(_, (n, _, _))| n == field_name)
                        .map(|(idx, (n, ty, _))| (idx as u32, n.clone(), *ty))
                })
        });
        let Some((idx, canonical, raw_field_type)) = found else {
            return (0, field_name.to_string(), None);
        };

        let field_type = if type_args.is_empty() {
            raw_field_type
        } else {
            let substitution: crate::hashmap::IndexMap<u32, TypeId> = (0..type_args.len() as u32)
                .zip(type_args.iter().copied())
                .collect();
            self.tysys
                .type_table
                .borrow_mut()
                .substitute_type_params(raw_field_type, &substitution)
        };
        (idx, canonical, Some(field_type))
    }

    /// Run `body` with reify's module perspective swapped to `module`, restoring
    /// it afterward. Reifying an AST fragment from another module (an associated
    /// constant's body, say) needs this because the `ann_*` accessors key on
    /// `current_module_source`. A no-op when `module` is the current one or is
    /// not loaded.
    fn with_const_module_perspective<R>(
        &mut self,
        module: &ModuleSource,
        body: impl FnOnce(&mut Self) -> R,
    ) -> R {
        let swap = if module == &self.current_module_source {
            None
        } else {
            match (
                self.loaded_modules.get(module),
                self.all_module_semantics.get(module),
            ) {
                (Some(m), Some(sem)) => Some((m.items.as_slice(), sem)),
                _ => None,
            }
        };
        let saved = swap.map(|(items, sem)| {
            (
                std::mem::replace(&mut self.current_module_source, module.clone()),
                std::mem::replace(&mut self.current_module_items, items),
                std::mem::replace(&mut self.sem, sem),
            )
        });
        let result = body(self);
        if let Some((src, items, sem)) = saved {
            self.current_module_source = src;
            self.current_module_items = items;
            self.sem = sem;
        }
        result
    }

    /// The concrete stand-in for an unresolved generic variant type: the
    /// caller's expected type, when it names the same variant with its type
    /// params filled in.
    fn resolved_variant_type(&self, recorded: TypeId, expected: Option<TypeId>) -> Option<TypeId> {
        let expected = expected?;
        let table = self.tysys.type_table.borrow();
        if table.contains_type_param(expected) {
            return None;
        }
        let recorded_is_concrete =
            matches!(table.get(recorded), ResolvedType::GenericInstance { .. })
                && !table.contains_type_param(recorded);
        if recorded_is_concrete {
            return None;
        }
        (table.base_type_name(recorded) == table.base_type_name(expected)).then_some(expected)
    }

    /// Reify a bare identifier reference. Local lookup goes through
    /// the per-function context (`FunctionContext::lookup`, the walk-order
    /// invariant). Non-local idents (globals, function refs, enum / variant
    /// ctors) read [`super::sem::ModuleDecls`] to pick the right TIR shape.
    fn reify_ident(
        &mut self,
        ident: &ast::IdentExpr,
        recorded_type: TypeId,
        expected_type: Option<TypeId>,
        ctx: &mut FunctionContext,
    ) -> TirExpr {
        use crate::tir::TirExprKind;

        // Default-argument parameter substitution: while reifying a default
        // expression, a free reference to an earlier parameter resolves to the
        // caller's already-reified argument (kept under the caller's
        // perspective). `reify_expr` clears this map before descending into a
        // binder / control form (closure, block, …) — exactly the forms
        // `Expr::substitute_idents` leaves untouched on the annotate side — so
        // a reference shadowed by an inner binding is never reached here. See
        // `reify_pad_args_with_defaults`.
        if !self.default_arg_overrides.is_empty()
            && let Some(tir) = self.default_arg_overrides.get(&ident.name)
        {
            return tir.clone();
        }

        // Canonicalize `ns::member` to its `ns$member` alias, matching
        // `resolve_ident` at annotate time (expr.rs) so reify consults the
        // same alias-keyed registries. The original `id` / `span` are kept.
        let canonical_ident;
        let ident = if let Some(canon) = self.sem.imports.canonical_ns_ref(&ident.name) {
            canonical_ident = ast::IdentExpr {
                id: ident.id,
                name: canon,
                segments: ident.segments.clone(),
                type_args: ident.type_args.clone(),
                type_args_on_prefix: ident.type_args_on_prefix,
                span: ident.span,
            };
            &canonical_ident
        } else {
            ident
        };

        // 1. Local / capture lookup, mirroring `resolve_ident`
        //    Use the local's stored type instead of
        //    `recorded_type`: the binding's own type is authoritative
        //    for an ident and does not depend on the recorded
        //    per-expression annotation. (Historically this also worked
        //    around template-string sub-parsers restarting `next_ast_id`
        //    at 0 and colliding on `AstId(0)`; sub-parsers now continue
        //    the parent's `AstIdSpace` + counter, see
        //    `parse_interpolation_expr`.)
        if let Some(var_ref) = ctx.lookup_or_capture(&ident.name) {
            match var_ref {
                super::types::VarRef::Local { index, type_id, .. } => {
                    return TirExpr::new(
                        TirExprKind::Local {
                            index,
                            name: ident.name.clone(),
                        },
                        type_id,
                        ident.span,
                    );
                }
                super::types::VarRef::Capture { index, type_id, .. } => {
                    return TirExpr::new(
                        TirExprKind::Capture {
                            index,
                            name: ident.name.clone(),
                        },
                        type_id,
                        ident.span,
                    );
                }
                super::types::VarRef::DerefCapture {
                    index,
                    ref_type_id,
                    inner_type_id,
                    ..
                } => {
                    let capture_expr = TirExpr::new(
                        TirExprKind::Capture {
                            index,
                            name: format!("__deref_cap_{index}"),
                        },
                        ref_type_id,
                        ident.span,
                    );
                    return TirExpr::new(
                        TirExprKind::Unary {
                            op: crate::tir::TirUnaryOp::Deref,
                            expr: Box::new(capture_expr),
                        },
                        inner_type_id,
                        ident.span,
                    );
                }
            }
        }

        // 2. Current-module global.
        if self
            .sem
            .decls
            .current_module_globals
            .contains_key(&ident.name)
        {
            return TirExpr::new(
                TirExprKind::GlobalVarGet {
                    module_source: self.current_module_source.clone(),
                    name: ident.name.clone(),
                },
                recorded_type,
                ident.span,
            );
        }

        // 3. Imported global.
        if let Some((src, original_name, _ty, _is_mut)) =
            self.sem.decls.imported_globals.get(&ident.name)
        {
            return TirExpr::new(
                TirExprKind::GlobalVarGet {
                    module_source: src.clone(),
                    name: original_name.clone(),
                },
                recorded_type,
                ident.span,
            );
        }

        // 3b. Current-module global declared in the AST but absent from
        //     `current_module_globals` — the case when reify walks a swapped-in
        //     callee module whose `ModuleSemantics` came from the stdlib
        //     snapshot, which does not rehydrate that map. Resolve it from
        //     `current_module_items`; otherwise it lowers to `()`.
        if let Some(global_decl) = self
            .current_module_items
            .iter()
            .find_map(|item| match item {
                ast::Item::Global(g) if g.name == ident.name => Some(g),
                _ => None,
            })
        {
            // The fact is genuinely absent here: branch 2 above already
            // returned for any global present in `current_module_globals`, and
            // this branch only fires for a snapshot-rehydrated callee module,
            // which carries no `current_module_globals`. So resolve the declared
            // type from the AST — the one re-resolution the completeness rule
            // sanctions (WEP 2026-05-26 §"Reify — mechanical"), and reify's
            // only `resolve_type` call site.
            let ty = self.resolve_type(&global_decl.ty);
            return TirExpr::new(
                TirExprKind::GlobalVarGet {
                    module_source: self.current_module_source.clone(),
                    name: ident.name.clone(),
                },
                ty,
                ident.span,
            );
        }

        // 4. Associated constant (e.g. `f64::PI`, `i32::MAX`). The
        //    elaborator inlines these to the resolved expression at
        //    every use site; reify reproduces the same inlining by
        //    re-reifying the constant's `Expr` from
        //    `sem.decls.associated_constants`. The constant's body is
        //    independent of the call site's scope (a pure literal /
        //    static expression in practice), so reify uses the
        //    surrounding `ctx` directly — matches the elaborator's
        //    `resolve_expr(&const_expr, ctx, …)`.
        if let Some(AssocConstSig {
            module: const_module,
            ty: type_id,
            value: const_expr,
            ..
        }) = self.associated_constant_of_path(ident)
        {
            // The constant's body lives in its *defining* module (e.g.
            // `pub const MAX: i32 = 2147483647;` in primitive.wado). Its
            // `AstId`s index that module's `ModuleSemantics`, not the use
            // site's, and `AstId`s are only unique within a module — so
            // reifying the body under `self.sem` (the current module) can
            // pick up a colliding `AstId`'s recorded type and mis-type the
            // literal (e.g. `i32::MAX`'s `2147483647` as an f64). Reify the
            // body under the defining module's perspective so every
            // annotation lookup hits the right module's records.
            let resolved = ctx.with_caller_bindings_hidden(|ctx| {
                self.with_const_module_perspective(&const_module, |this| {
                    this.reify_expr(&const_expr, ctx, Some(type_id))
                })
            });
            return TirExpr::new(resolved.kind, type_id, ident.span);
        }

        // 4b. Primitive associated constant (`i32::MAX`, `u8::MIN`, …) that
        //     is not in `associated_constants`. This happens when reify is
        //     walking a swapped-in callee module (a default-argument
        //     expression — e.g. `max_output: i32 = i32::MAX`) whose
        //     `ModuleSemantics` came from the stdlib snapshot, which does
        //     not rehydrate `associated_constants`. The value is a compile
        //     -time constant of the named primitive type, so emit it as a
        //     typed integer literal directly.
        if let Some((prefix, suffix)) = ident.name.split_once("::")
            && !suffix.contains("::")
            && let Some((value, prim_type)) = primitive_int_assoc_const(prefix, suffix)
        {
            return TirExpr::new(
                TirExprKind::IntLiteral {
                    value: value as u64,
                    repr: value.to_string(),
                },
                prim_type,
                ident.span,
            );
        }

        // 5. Free function reference — the ident names a function in
        //    the current module or imported via a `use` declaration.
        //    Emit `TirExprKind::FuncRef` with the recorded
        //    instantiation's type_args when present. A bare case (`None`)
        //    resolves to its declaration, which is no function; it is the
        //    case below.
        let is_bare_case = self.ann_bare_case(ident.id).is_some();
        if !is_bare_case
            && self
                .sem
                .decls
                .function_return_types
                .contains_key(&ident.name)
        {
            let type_args = self
                .ann_generic_instantiations(ident.id)
                .map(|gi| gi.type_args)
                .unwrap_or_default();
            return TirExpr::new(
                TirExprKind::FuncRef {
                    module_source: self.current_module_source.clone(),
                    name: ident.name.clone(),
                    type_args,
                },
                recorded_type,
                ident.span,
            );
        }
        if !is_bare_case && let Some(def) = self.tysys.resolutions.declared_if_walked(ident.id) {
            let (import_src, original_name) = {
                let defs = self.tysys.resolutions.defs();
                (defs.module(def).clone(), defs.name(def).to_string())
            };
            // The scope answers for types and functions alike; the type /
            // variant / enum / flags / resource cases were already handled
            // above and would have returned. Anything left here is a
            // function import.
            let type_args = self
                .ann_generic_instantiations(ident.id)
                .map(|gi| gi.type_args)
                .unwrap_or_default();
            return TirExpr::new(
                TirExprKind::FuncRef {
                    module_source: import_src,
                    name: original_name,
                    type_args,
                },
                recorded_type,
                ident.span,
            );
        }

        // 5b. Imported free function reference resolved through the symbol
        //     table (covers namespace-import functions, whose `ns$fn` aliases
        //     name functions rather than types). Mirrors annotate's
        //     `resolve_func_ref_ident` → `lookup_func_ast_for_ref` and emits a
        //     `FuncRef` keyed by the function's defining module + original name.
        if self.sem.decls.imported_functions.contains(&ident.name)
            && let Some(symbol) = self.symbol_at(ident.id)
            && matches!(symbol.kind, crate::symbol::SymbolKind::Function(_))
        {
            let type_args = self
                .ann_generic_instantiations(ident.id)
                .map(|gi| gi.type_args)
                .unwrap_or_default();
            return TirExpr::new(
                TirExprKind::FuncRef {
                    module_source: symbol.module_source().clone(),
                    name: symbol.name.clone(),
                    type_args,
                },
                recorded_type,
                ident.span,
            );
        }

        // 6. Qualified case path `Type::Case`. Variant / enum / flags
        //    are checked in the same priority order as
        //    `Elaborator::resolve_ident`. The
        //    namespace-import form `ns::Type::Case` (two `::`
        //    separators) is handled by a dedicated branch in the
        //    elaborator that resolves the namespace alias first.
        if let Some((owner, spelled)) = self.case_path(ident)
            && let Some((_, suffix)) = spelled.split_once("::")
        {
            // Two-segment qualified path is "Type::Case". Anything with
            // a further `::` is `ns::Type::Case` (namespace path) —
            // defer to a later branch.
            if !suffix.contains("::") {
                let lookup = self.type_lookup();

                // A newtype reaches its base's members and keeps its own type:
                // `C::Green` is the implicit `Color::Green as C`.
                let through_newtype = owner
                    .and_then(|def| super::types::newtype_member_owner(&lookup, &self.tysys, def));
                let owner = through_newtype.map(|(base, _)| base).or(owner);

                // Variant case.
                if let Some(variant_info) = owner
                    .and_then(|owner| lookup.variant_cases_of(owner))
                    .cloned()
                    && let Some((case_index, case_data)) = variant_info
                        .cases
                        .iter()
                        .enumerate()
                        .find(|(_, c)| c.name == suffix)
                        .map(|(i, c)| (i, c.clone()))
                {
                    // Only generic variants record an instance type +
                    // type_args; for a non-generic one the bare
                    // `recorded_type` already names the right `Variant`.
                    // A payload-less case carries no value to infer from, so
                    // annotate can only record the decl's own `V<T>`. In a
                    // struct-literal field the caller knows the substituted
                    // `V<i32>`; prefer it over the unresolved record.
                    let recorded_variant_type = self
                        .ann_generic_instantiations(ident.id)
                        .map(|gi| gi.instance_type)
                        .unwrap_or(recorded_type);
                    let variant_type = self
                        .resolved_variant_type(recorded_variant_type, expected_type)
                        .unwrap_or(recorded_variant_type);
                    return TirExpr::new(
                        TirExprKind::VariantConstruct {
                            variant_type,
                            case_index: case_index as u32,
                            case_name: case_data.name,
                            payload: None,
                        },
                        through_newtype.map_or(variant_type, |(_, named)| named),
                        ident.span,
                    );
                }

                // Enum case.
                if let Some(enum_info) =
                    owner.and_then(|owner| lookup.enum_cases_of(owner)).cloned()
                    && let Some(case_data) = enum_info.find_case(suffix).cloned()
                {
                    let enum_type = self
                        .tysys
                        .type_table
                        .borrow()
                        .type_id_of_decl(enum_info.defined_at);
                    return TirExpr::new(
                        crate::tir::TirExprKind::EnumConstruct {
                            enum_type,
                            case_index: case_data.index,
                            case_name: case_data.name,
                        },
                        through_newtype.map_or(enum_type, |(_, named)| named),
                        ident.span,
                    );
                }

                // Flags member.
                if let Some(flags_info) = owner
                    .and_then(|owner| lookup.flags_members_of(owner))
                    .cloned()
                    && let Some(member) = flags_info
                        .members
                        .iter()
                        .find(|m| m.name == suffix)
                        .cloned()
                {
                    return TirExpr::new(
                        TirExprKind::IntLiteral {
                            value: u64::from(member.bitmask),
                            repr: member.bitmask.to_string(),
                        },
                        through_newtype.map_or(flags_info.type_id, |(_, named)| named),
                        ident.span,
                    );
                }
            }
        }

        // No remaining recognised ident kind — the elaborator would
        // have diagnosed an unknown identifier at annotate time.
        // Match the elaborator's recovery shape so reify doesn't
        // panic on a known-bad input.
        let _ = recorded_type;
        TirExpr::new(TirExprKind::Unit, crate::tir::TypeTable::ERROR, ident.span)
    }

    /// Replay an `i128` / `u128` numeric-literal coercion recorded by annotate,
    /// returning `None` for every other shape. The 128-bit types are prelude
    /// structs, so the value is materialized by a `from_u64` / `from_i64` /
    /// `from_pair` call; every other `NumericLiteral` coercion is free, the
    /// literal already carrying its coerced type.
    fn try_reify_int128_coercion(&self, expr: &ast::Expr) -> Option<TirExpr> {
        let choice = self.ann_coercions(expr.id())?;
        if choice.kind != super::sem::types::CoercionKind::NumericLiteral {
            return None;
        }
        let target_type = choice.target_type;
        let name = match self.tysys.type_table.borrow().get(target_type).clone() {
            crate::tir::ResolvedType::Struct { def, .. }
                if matches!(
                    self.tysys
                        .type_table
                        .borrow()
                        .struct_head_name(def)
                        .as_str(),
                    "u128" | "i128"
                ) =>
            {
                self.tysys
                    .type_table
                    .borrow()
                    .fq_base_type_name(target_type)
            }
            _ => return None,
        };

        // Plain literal, or the negated `-NUM` shape whose coercion is
        // keyed on the enclosing `Unary` node.
        let (repr, negated) = match expr {
            ast::Expr::Literal(ast::LiteralExpr {
                value: ast::Literal::Number(repr),
                ..
            }) => (repr.clone(), false),
            ast::Expr::Unary(unary) if unary.op == ast::UnaryOp::Neg => match &unary.expr {
                ast::Expr::Literal(ast::LiteralExpr {
                    value: ast::Literal::Number(repr),
                    ..
                }) => (repr.clone(), true),
                _ => return None,
            },
            _ => return None,
        };

        let parse_result = if name.decl_name() == "u128" {
            super::util::parse_u128_literal(&repr).map(|v| v as i128)
        } else if negated {
            super::util::parse_i128_literal(&format!("-{repr}"))
        } else {
            super::util::parse_i128_literal(&repr)
        };
        let value = parse_result.ok()?;

        Some(build_int128_literal_call(
            &name,
            value,
            &repr,
            !negated,
            target_type,
            expr.span(),
        ))
    }

    /// Replay an `expr as i128/u128` cast, modulo newtypes of one. `None` for
    /// any other target; a non-numeric operand yields the bare cast.
    fn try_reify_int128_cast(
        &mut self,
        cast: &ast::CastExpr,
        target_type: TypeId,
        ctx: &mut FunctionContext,
    ) -> Option<TirExpr> {
        let target_base = self
            .tysys
            .type_table
            .borrow()
            .representation_head(target_type);
        let name = match self.tysys.type_table.borrow().get(target_base).clone() {
            crate::tir::ResolvedType::Struct { def, .. }
                if matches!(
                    self.tysys
                        .type_table
                        .borrow()
                        .struct_head_name(def)
                        .as_str(),
                    "u128" | "i128"
                ) =>
            {
                self.tysys
                    .type_table
                    .borrow()
                    .fq_base_type_name(target_base)
            }
            _ => return None,
        };

        // Literal operand: `1042 as u128`.
        if let ast::Expr::Literal(lit) = &cast.expr
            && let Some(repr) = super::expr::int_literal_repr(lit)
        {
            let parsed = if name.decl_name() == "u128" {
                super::util::parse_u128_literal(repr).map(|v| v as i128)
            } else {
                super::util::parse_i128_literal(repr)
            };
            if let Ok(value) = parsed {
                return Some(build_int128_literal_call(
                    &name,
                    value,
                    repr,
                    true,
                    target_type,
                    cast.span,
                ));
            }
        }

        // Negated literal operand (i128 only): `-170... as i128`.
        if name.decl_name() == "i128"
            && let ast::Expr::Unary(unary) = &cast.expr
            && unary.op == ast::UnaryOp::Neg
            && let ast::Expr::Literal(ast::LiteralExpr {
                value: ast::Literal::Number(repr),
                ..
            }) = &unary.expr
            && !super::util::is_float_only_literal(repr)
            && let Ok(value) = super::util::parse_i128_literal(&format!("-{repr}"))
        {
            return Some(build_int128_literal_call(
                &name,
                value,
                repr,
                false,
                target_type,
                unary.span,
            ));
        }

        // General numeric operand: `x as u128` →
        // `u128::from_u64(x as u64)`. `inner` is reified once here; a
        // non-numeric operand (no valid construction) emits the bare cast
        // directly rather than re-reifying through the caller's fallback.
        let inner = self.reify_expr(&cast.expr, ctx, None);
        let source_is_numeric = {
            let tt = self.tysys.type_table.borrow();
            tt.is_integer(inner.type_id) || tt.is_float(inner.type_id)
        };
        if !source_is_numeric {
            return Some(TirExpr::new(
                crate::tir::TirExprKind::Cast {
                    expr: Box::new(inner),
                    target_type,
                },
                target_type,
                cast.span,
            ));
        }
        let intermediate_type = if name.decl_name() == "u128" {
            crate::tir::TypeTable::U64
        } else {
            crate::tir::TypeTable::I64
        };
        let casted = TirExpr::new(
            crate::tir::TirExprKind::Cast {
                expr: Box::new(inner),
                target_type: intermediate_type,
            },
            intermediate_type,
            cast.span,
        );
        Some(build_int128_from_intermediate(
            &name,
            casted,
            target_type,
            cast.span,
        ))
    }

    /// `i128/u128 as T` for a wide-int *source*. The 128-bit types are
    /// prelude structs, so a bare `Cast` would leak the boxed struct ref
    /// into a slot expecting a Wasm scalar (issue #1328). Lower instead to
    /// prelude calls: `f64`/`f32` through the correctly rounded
    /// `as_f64`/`as_f32`, integer targets through `low()` plus a primitive
    /// cast (truncation), and `i128 ↔ u128` through the bit-reinterpreting
    /// `from_u128`/`from_i128` constructors. Targets outside that set
    /// return `None`; `resolve_cast` has already reported them as invalid.
    fn try_reify_int128_source_cast(
        &mut self,
        cast: &ast::CastExpr,
        target_type: TypeId,
        ctx: &mut FunctionContext,
    ) -> Option<TirExpr> {
        use crate::compiler_item::CompilerItem;
        use crate::tir::{PrimitiveType, ResolvedType, TypeTable};

        let source_type = self.ann_expression_types(cast.expr.id())?;
        // Newtypes share their base's representation, so dispatch on the
        // ultimate base of both sides; explicit repr-compatible `Cast`
        // nodes bridge the newtype boundaries below.
        let (source_base, target_base) = {
            let tt = self.tysys.type_table.borrow();
            (
                tt.representation_head(source_type),
                tt.representation_head(target_type),
            )
        };
        let source_name = match self.tysys.type_table.borrow().get(source_base).clone() {
            ResolvedType::Struct { def, .. }
                if matches!(
                    self.tysys
                        .type_table
                        .borrow()
                        .struct_head_name(def)
                        .as_str(),
                    "u128" | "i128"
                ) =>
            {
                self.tysys.type_table.borrow().struct_head_name(def)
            }
            _ => return None,
        };
        let signed_source = source_name == "i128";

        enum Lowering {
            /// `i128 as i128` / `u128 as u128` — no-op.
            Identity,
            /// `&self` accessor returning the target primitive directly.
            Method(CompilerItem),
            /// `low()` then a primitive cast down to the target width.
            LowThenCast,
            /// Static bit-reinterpreting constructor of the other wide type.
            Reinterpret(CompilerItem),
        }
        let lowering = match self.tysys.type_table.borrow().get(target_base).clone() {
            ResolvedType::Primitive(PrimitiveType::F64) => Lowering::Method(if signed_source {
                CompilerItem::I128AsF64
            } else {
                CompilerItem::U128AsF64
            }),
            ResolvedType::Primitive(PrimitiveType::F32) => Lowering::Method(if signed_source {
                CompilerItem::I128AsF32
            } else {
                CompilerItem::U128AsF32
            }),
            ResolvedType::Primitive(
                PrimitiveType::I64
                | PrimitiveType::U64
                | PrimitiveType::I32
                | PrimitiveType::U32
                | PrimitiveType::I16
                | PrimitiveType::U16
                | PrimitiveType::I8
                | PrimitiveType::U8,
            ) => Lowering::LowThenCast,
            ResolvedType::Struct { def, .. }
                if self.tysys.type_table.borrow().struct_head_name(def) == source_name =>
            {
                if target_type == source_type {
                    Lowering::Identity
                } else {
                    // Same wide base but a newtype on either side: the
                    // bare `Cast` emitted by the caller is the correct
                    // repr-compatible reinterpret.
                    return None;
                }
            }
            ResolvedType::Struct { def, .. }
                if self.tysys.type_table.borrow().struct_head_name(def) == "i128" =>
            {
                Lowering::Reinterpret(CompilerItem::I128FromU128)
            }
            ResolvedType::Struct { def, .. }
                if self.tysys.type_table.borrow().struct_head_name(def) == "u128" =>
            {
                Lowering::Reinterpret(CompilerItem::U128FromI128)
            }
            _ => return None,
        };

        let make_func_ref = |tysys: &super::tysys::TypeSystem, item: CompilerItem| {
            let (owner_head, method_name) = {
                let tt = tysys.type_table.borrow();
                let (_, _, method_name) = tt.compiler_method(item);
                (
                    tt.compiler_items().require_method_owner(item).clone(),
                    method_name.to_string(),
                )
            };
            let method_info = crate::name::LocalMethodName::new(owner_head, None, method_name);
            crate::tir::FunctionRef {
                module_source: crate::module_source::ModuleSource::int128(),
                name: method_info.to_mangled_name(),
                monomorph_info: None,
                method_info: Some(method_info),
            }
        };
        // Repr-compatible `Cast` bridging a newtype boundary (no-op in
        // codegen); identity when the types already match.
        let bridge = |expr: TirExpr, to: TypeId, span: crate::token::Span| {
            if expr.type_id == to {
                return expr;
            }
            TirExpr::new(
                crate::tir::TirExprKind::Cast {
                    expr: Box::new(expr),
                    target_type: to,
                },
                to,
                span,
            )
        };

        let inner = self.reify_expr(&cast.expr, ctx, None);
        let span = cast.span;
        // A newtype source first reinterprets to its wide base so the
        // prelude calls below see their declared receiver/argument type.
        let inner = bridge(inner, source_base, span);
        match lowering {
            Lowering::Identity => Some(inner),
            Lowering::Method(item) => {
                let func = make_func_ref(&self.tysys, item);
                let receiver = adjust_receiver_for_self_kind(
                    inner,
                    ast::SelfKind::Ref,
                    /* is_ref_impl */ false,
                    span,
                    &self.tysys.type_table,
                );
                let call = build_tir_method_call(receiver, func, vec![], vec![], target_base, span);
                Some(bridge(call, target_type, span))
            }
            Lowering::LowThenCast => {
                let item = if signed_source {
                    CompilerItem::I128Low
                } else {
                    CompilerItem::U128Low
                };
                let func = make_func_ref(&self.tysys, item);
                let receiver = adjust_receiver_for_self_kind(
                    inner,
                    ast::SelfKind::Ref,
                    /* is_ref_impl */ false,
                    span,
                    &self.tysys.type_table,
                );
                let low_call =
                    build_tir_method_call(receiver, func, vec![], vec![], TypeTable::U64, span);
                let converted = if target_base == TypeTable::U64 {
                    low_call
                } else {
                    TirExpr::new(
                        crate::tir::TirExprKind::Cast {
                            expr: Box::new(low_call),
                            target_type: target_base,
                        },
                        target_base,
                        span,
                    )
                };
                Some(bridge(converted, target_type, span))
            }
            Lowering::Reinterpret(item) => {
                let func = make_func_ref(&self.tysys, item);
                let call = TirExpr::new(
                    crate::tir::TirExprKind::Call {
                        func: Box::new(func),
                        type_args: vec![],
                        args: vec![crate::tir::CallArg::new(inner, false)],
                        has_receiver: false,
                    },
                    target_base,
                    span,
                );
                Some(bridge(call, target_type, span))
            }
        }
    }

    fn reify_literal(
        &mut self,
        lit: &ast::LiteralExpr,
        recorded_type: TypeId,
        ctx: &FunctionContext,
    ) -> TirExpr {
        use crate::tir::{TirExprKind, TypeTable};
        let kind = match &lit.value {
            ast::Literal::Number(repr) => {
                // The *recorded type* decides Int vs Float TIR literal; the
                // literal's *syntactic form* (`is_float_only_literal`) decides
                // how to read its value, so `let x: f64 = 0xFF` reads as an
                // integer then converts. Peel newtypes first, or a float literal
                // bound to a float-newtype target takes the integer path.
                let base_target = self
                    .tysys
                    .type_table
                    .borrow()
                    .representation_head(recorded_type);
                // A float-only literal (`1.0`, `0.0`, `1e2`) is a float
                // regardless of the recorded type: when the recorded type is
                // missing/UNKNOWN (e.g. a stdlib const body whose
                // `expression_types` entry is absent from the cached
                // snapshot) the syntactic form is authoritative, matching
                // production's `resolve_numeric_literal`. An
                // integer literal still defers to the recorded type so
                // `let x: f64 = 1` takes the float path via `is_float_target`.
                let is_float_target = base_target == TypeTable::F32
                    || base_target == TypeTable::F64
                    || (recorded_type == TypeTable::UNKNOWN
                        && super::util::is_float_only_literal(repr));
                if is_float_target {
                    let value: f64 = if super::util::is_float_only_literal(repr) {
                        super::util::parse_float_literal(repr).unwrap_or(0.0)
                    } else {
                        super::util::parse_u128_literal(repr)
                            .map(|v| v as f64)
                            .unwrap_or(0.0)
                    };
                    // The literal's *type* must be a concrete float, not the
                    // (possibly UNKNOWN) recorded type: a float-only literal
                    // with no recorded type defaults to `f64` (matching
                    // production's `resolve_numeric_literal`). Leaving it
                    // UNKNOWN makes lowering pick an integer op for the
                    // surrounding arithmetic (`f64.div` -> `i32.div_s`).
                    // f32 target keeps f32; otherwise (f64 target, or no
                    // recorded type) an untyped float literal defaults to f64.
                    let float_type = if base_target == TypeTable::F32 {
                        TypeTable::F32
                    } else {
                        TypeTable::F64
                    };
                    return TirExpr::new(
                        TirExprKind::FloatLiteral {
                            value,
                            repr: repr.clone(),
                        },
                        float_type,
                        lit.span,
                    );
                } else {
                    let value = super::util::parse_u128_literal(repr).unwrap_or(0) as u64;
                    TirExprKind::IntLiteral {
                        value,
                        repr: repr.clone(),
                    }
                }
            }
            ast::Literal::String(s) => {
                // Decode escape sequences (`\"`, `\n`, `\\`, …) the same
                // way the elaborator does — the AST holds
                // the raw source text. Without this a literal like
                // `"{\""` reaches codegen with the backslash intact and
                // serializes as `{\"` instead of `{"`.
                let value = super::util::unescape_string(s).unwrap_or_default();
                TirExprKind::StringLiteral(value)
            }
            ast::Literal::Bytes(raw) => {
                // Decode the raw source to bytes and reuse the `#include_bytes`
                // lowering (`BytesLiteral` -> byte-buffer data segment).
                let bytes = super::util::unescape_bytes(raw).unwrap_or_default();
                let byte_list_type = if recorded_type == crate::tir::TypeTable::UNKNOWN {
                    self.tysys.type_table.borrow_mut().make_byte_list()
                } else {
                    recorded_type
                };
                return TirExpr::new(TirExprKind::BytesLiteral(bytes), byte_list_type, lit.span);
            }
            ast::Literal::Char(s) => {
                // The Char literal is the raw source text (e.g. `'a'`,
                // `'\n'`). Decode escapes via the shared `unescape_char`,
                // matching the elaborator — a hand-rolled
                // `chars().next()` reads the backslash of `'\n'` as `'\'`,
                // which then fails to match a `'\n'` pattern that decodes
                // correctly.
                let ch = super::util::unescape_char(s).unwrap_or('\0');
                TirExprKind::CharLiteral(ch)
            }
            ast::Literal::Byte(s) => {
                let byte = super::util::unescape_byte(s).unwrap_or(0);
                let byte_type = if recorded_type == crate::tir::TypeTable::UNKNOWN {
                    crate::tir::TypeTable::U8
                } else {
                    recorded_type
                };
                return TirExpr::new(
                    TirExprKind::IntLiteral {
                        value: u64::from(byte),
                        repr: s.clone(),
                    },
                    byte_type,
                    lit.span,
                );
            }
            ast::Literal::Bool(b) => TirExprKind::BoolLiteral(*b),
            ast::Literal::Null => TirExprKind::Null,
            ast::Literal::Unit => TirExprKind::Unit,
            ast::Literal::LocationFunction => {
                // `#function`; in a default, the calling function. Rendered,
                // so an operation's default body reports the operation rather
                // than the synthesized name its body is stored under.
                let name = match &self.call_site_location {
                    Some(loc) => loc.function_name.clone(),
                    None => ctx.function_name.clone(),
                };
                TirExprKind::StringLiteral(crate::name::display_function_name(&name))
            }
            ast::Literal::LocationFile => {
                // `#file`; in a default, the caller's module.
                let string_type = self
                    .tysys
                    .type_table
                    .borrow_mut()
                    .make_compiler_struct(crate::compiler_item::CompilerItem::String);
                let file = match &self.call_site_location {
                    Some(loc) => loc.module.to_string(),
                    None => self.current_module_source.to_string(),
                };
                return TirExpr::new(TirExprKind::StringLiteral(file), string_type, lit.span);
            }
            ast::Literal::LocationLine => {
                // `#line`, 1-indexed (`I32`); in a default, the call-site line.
                let line = match &self.call_site_location {
                    Some(loc) => loc.span.line as u64,
                    None => lit.span.line as u64,
                };
                return TirExpr::new(
                    TirExprKind::IntLiteral {
                        value: line,
                        repr: line.to_string(),
                    },
                    crate::tir::TypeTable::I32,
                    lit.span,
                );
            }
            ast::Literal::DataSection => {
                // `#data` — the loaded module's `__DATA__` section.
                let string_type = self
                    .tysys
                    .type_table
                    .borrow_mut()
                    .make_compiler_struct(crate::compiler_item::CompilerItem::String);
                let data = self
                    .loaded_modules
                    .get(&self.current_module_source)
                    .and_then(|m| m.data_section())
                    .map(str::to_owned)
                    .unwrap_or_default();
                return TirExpr::new(TirExprKind::StringLiteral(data), string_type, lit.span);
            }
            ast::Literal::IncludeStr(raw_path) => {
                let string_type = self
                    .tysys
                    .type_table
                    .borrow_mut()
                    .make_compiler_struct(crate::compiler_item::CompilerItem::String);
                let key = [self.current_module_source.to_string(), raw_path.clone()];
                let value = self
                    .tysys
                    .included_files
                    .get(&key)
                    .and_then(|bytes| std::str::from_utf8(bytes).ok())
                    .map(str::to_owned)
                    .unwrap_or_default();
                return TirExpr::new(TirExprKind::StringLiteral(value), string_type, lit.span);
            }
            ast::Literal::IncludeBytes(raw_path) => {
                let array_u8_type = if recorded_type == crate::tir::TypeTable::UNKNOWN {
                    self.tysys.type_table.borrow_mut().make_byte_list()
                } else {
                    recorded_type
                };
                let key = [self.current_module_source.to_string(), raw_path.clone()];
                let bytes = self
                    .tysys
                    .included_files
                    .get(&key)
                    .cloned()
                    .unwrap_or_default();
                return TirExpr::new(TirExprKind::BytesLiteral(bytes), array_u8_type, lit.span);
            }
        };
        TirExpr::new(kind, recorded_type, lit.span)
    }

    /// Resolve a nullary qualified pattern (`TokenKind::FOO`, `i32::MAX`) to its
    /// associated-constant value. An integer constant becomes a `Literal`
    /// pattern, signed per the scrutinee, so it benefits from switch lowering;
    /// everything else becomes a `ConstantValue`. `None` when the name is not a
    /// recorded associated constant — i.e. it is a real variant case.
    fn reify_associated_const_pattern(
        &mut self,
        variant_name: &str,
        variant_qualifier: Option<&ast::Type>,
        scrutinee_type: TypeId,
        span: crate::token::Span,
        ctx: &mut FunctionContext,
    ) -> Option<TirPattern> {
        use crate::tir::{ResolvedType, TirExpr, TirExprKind, TirLiteralPattern};

        let AssocConstSig {
            module: const_module,
            ty: type_id,
            value: const_expr,
            ..
        } = self.associated_constant_qualified(variant_qualifier, variant_name)?;

        // Reify the body under its defining module so colliding cross-module
        // `AstId`s can't mis-type the inlined constant (see `reify_ident`).
        let resolved = self.with_const_module_perspective(&const_module, |this| {
            this.reify_expr(&const_expr, ctx, Some(type_id))
        });
        match &resolved.kind {
            TirExprKind::IntLiteral { repr, .. } => {
                let is_unsigned = matches!(
                    self.tysys.type_table.borrow().get(scrutinee_type),
                    ResolvedType::Primitive(
                        crate::tir::PrimitiveType::U8
                            | crate::tir::PrimitiveType::U16
                            | crate::tir::PrimitiveType::U32
                            | crate::tir::PrimitiveType::U64
                            | crate::tir::PrimitiveType::U128
                    ),
                ) || matches!(
                    self.tysys.type_table.borrow().get(scrutinee_type),
                    ResolvedType::Struct { def, .. } if self.tysys.type_table.borrow().struct_head_name(*def) == "u128",
                );
                if is_unsigned {
                    if let Ok(v) = super::util::parse_u128_literal(repr) {
                        return Some(TirPattern::Literal(TirLiteralPattern::U128(v)));
                    }
                } else if let Ok(v) = super::util::parse_i128_literal(repr) {
                    return Some(TirPattern::Literal(TirLiteralPattern::I128(v)));
                }
            }
            TirExprKind::BoolLiteral(v) => {
                return Some(TirPattern::Literal(TirLiteralPattern::Bool(*v)));
            }
            TirExprKind::CharLiteral(v) => {
                return Some(TirPattern::Literal(TirLiteralPattern::Char(*v)));
            }
            _ => {}
        }
        Some(TirPattern::ConstantValue {
            expr: Box::new(TirExpr::new(resolved.kind, type_id, span)),
        })
    }

    /// Resolve a range-pattern endpoint to its `i128` value. Literal
    /// endpoints parse directly; an associated-constant endpoint
    /// (`i32::MIN`, `TokenKind::FOO`) resolves through
    /// `sem.decls.associated_constants` — mirroring the elaborator, which
    /// inlines const range bounds to their values.
    fn pattern_endpoint_value(
        &mut self,
        endpoint: &ast::Pattern,
        ctx: &mut FunctionContext,
    ) -> i128 {
        use crate::tir::TirExprKind;
        if let ast::Pattern::Variant {
            variant_name,
            variant_qualifier,
            bindings,
            ..
        } = endpoint
            && bindings.is_empty()
        {
            // Builtin primitive const (`i32::MIN`, `u8::MAX`): not in the
            // user `associated_constants` map, resolved by value.
            if let Some(v) =
                super::stmt::primitive_assoc_const_to_i128(variant_qualifier.as_ref(), variant_name)
            {
                return v;
            }
            if let Some(AssocConstSig {
                module: const_module,
                ty: type_id,
                value: const_expr,
                ..
            }) = self.associated_constant_qualified(variant_qualifier.as_ref(), variant_name)
            {
                let resolved = self.with_const_module_perspective(&const_module, |this| {
                    this.reify_expr(&const_expr, ctx, Some(type_id))
                });
                if let TirExprKind::IntLiteral { repr, .. } = &resolved.kind {
                    return super::util::parse_i128_literal(repr)
                        .or_else(|_| super::util::parse_u128_literal(repr).map(|v| v as i128))
                        .unwrap_or(0);
                }
            }
        }
        pattern_endpoint_to_i128(endpoint)
    }

    /// [`super::types::newtype_member_owner`] for the declaration `prefix`
    /// names at `site`.
    pub(super) fn newtype_member_owner(
        &self,
        site: Option<ast::AstId>,
        prefix: &str,
    ) -> Option<(crate::defs::DefId, TypeId)> {
        let lookup = self.type_lookup();
        let def = lookup.declaration_at(site, prefix)?;
        super::types::newtype_member_owner(&lookup, &self.tysys, def)
    }

    /// Whose cases a pattern names: the scrutinee's structure, with references
    /// and newtype links peeled. A newtype inherits its base's cases (WEP
    /// 2026-01-29), so `match c { Color::Green => … }` holds for a `C` too —
    /// reading the identity here dropped an enum into the variant branch, and
    /// WIR build then had a variant pattern over an `Enum`.
    pub(super) fn scrutinee_structure_head(&self, scrutinee_type: TypeId) -> TypeId {
        let tt = self.tysys.type_table.borrow();
        tt.reflect_structure_head(tt.peel_refs(scrutinee_type))
    }

    /// Discriminant index of `case_name` when `scrutinee_type` is an enum that
    /// declares it. Drives lowering an enum-case pattern to `TirPattern::Enum`.
    fn scrutinee_enum_case_index(&self, scrutinee_type: TypeId, case_name: &str) -> Option<u32> {
        use crate::tir::ResolvedType;
        // Peel references for match ergonomics: `match &c { Red => … }`
        // presents the scrutinee as `&Color`.
        let peeled = self.scrutinee_structure_head(scrutinee_type);
        if !matches!(
            self.tysys.type_table.borrow().get(peeled),
            ResolvedType::Enum { .. }
        ) {
            return None;
        }
        self.type_lookup()
            .enum_cases_of(self.tysys.type_def(peeled)?)?
            .case_index
            .get(case_name)
            .copied()
    }

    /// True when `scrutinee_type` is a variant (directly or as a generic
    /// instance) whose cases include `case_name`.
    fn scrutinee_has_variant_case(&self, scrutinee_type: TypeId, case_name: &str) -> bool {
        use crate::tir::ResolvedType;
        let peeled = self.scrutinee_structure_head(scrutinee_type);
        if !matches!(
            self.tysys.type_table.borrow().get(peeled),
            ResolvedType::Variant { .. } | ResolvedType::GenericInstance { .. }
        ) {
            return false;
        }
        self.tysys
            .type_def(peeled)
            .and_then(|def| self.type_lookup().variant_cases_of(def))
            .is_some_and(|info| info.cases.iter().any(|c| c.name == case_name))
    }

    /// Lower a nullary variant-case pattern (e.g. `None`) to
    /// `TirPattern::Variant` with no bindings, resolving the case's
    /// payload type from the scrutinee's variant decl. Shared by the
    /// bare-ident and qualified-`Variant` arms.
    fn reify_nullary_variant_case(
        &mut self,
        scrutinee_type: TypeId,
        case_name: &str,
    ) -> TirPattern {
        use crate::tir::ResolvedType;
        // Peel references (match ergonomics): `if let None = rn` with
        // `rn: &Option<T>` matches a nullary case through the reference.
        let peeled = self.tysys.type_table.borrow().peel_refs(scrutinee_type);
        let type_args = match self.tysys.type_table.borrow().get(peeled).clone() {
            ResolvedType::GenericInstance { type_args, .. } => type_args,
            _ => Vec::<TypeId>::new(),
        };
        let payload_type =
            self.get_variant_case_payload_type(self.tysys.type_def(peeled), case_name, &type_args);
        TirPattern::Variant {
            enum_type: peeled,
            variant_name: case_name.to_string(),
            bindings: vec![],
            payload_type,
        }
    }

    /// A bare ident naming an immutable global lowers to a
    /// `ConstantValue` comparison against that global rather than a
    /// binding (mirrors `Elaborator::resolve_if_pattern_inner`). Mutable
    /// globals are not constants and fall through to a binding.
    fn reify_immutable_global_pattern(
        &self,
        name: &str,
        span: crate::token::Span,
    ) -> Option<TirPattern> {
        use crate::tir::{TirExpr, TirExprKind};
        if let Some(&(ty, mutable)) = self.sem.decls.current_module_globals.get(name)
            && !mutable
        {
            return Some(TirPattern::ConstantValue {
                expr: Box::new(TirExpr::new(
                    TirExprKind::GlobalVarGet {
                        module_source: self.current_module_source.clone(),
                        name: name.to_string(),
                    },
                    ty,
                    span,
                )),
            });
        }
        if let Some((source_module, original_name, ty, mutable)) =
            self.sem.decls.imported_globals.get(name)
            && !*mutable
        {
            return Some(TirPattern::ConstantValue {
                expr: Box::new(TirExpr::new(
                    TirExprKind::GlobalVarGet {
                        module_source: source_module.clone(),
                        name: original_name.clone(),
                    },
                    *ty,
                    span,
                )),
            });
        }
        None
    }

    /// Wrap `inner` in the reference kind of `scrutinee_type` for match
    /// ergonomics. Walks the reference layers of the scrutinee: a `&mut`
    /// sets `&mut` unless a `&` is also present (most restrictive wins),
    /// matching `Elaborator::resolve_if_pattern`'s `RefBinding`. A
    /// non-reference scrutinee returns `inner` unchanged.
    fn apply_scrutinee_ref_kind(&self, scrutinee_type: TypeId, inner: TypeId) -> TypeId {
        use crate::tir::ResolvedType;
        let mut cur = scrutinee_type;
        let mut saw_ref = false;
        let mut saw_mut_ref = false;
        loop {
            let resolved = self.tysys.type_table.borrow().get(cur).clone();
            match resolved {
                ResolvedType::Ref(i) => {
                    saw_ref = true;
                    cur = i;
                }
                ResolvedType::MutRef(i) => {
                    saw_mut_ref = true;
                    cur = i;
                }
                _ => break,
            }
        }
        if saw_ref {
            self.tysys.type_table.borrow_mut().make_ref(inner)
        } else if saw_mut_ref {
            self.tysys.type_table.borrow_mut().make_mut_ref(inner)
        } else {
            inner
        }
    }

    pub(super) fn reify_pattern(
        &mut self,
        pattern: &ast::Pattern,
        scrutinee_type: TypeId,
        ctx: &mut FunctionContext,
    ) -> TirPattern {
        match pattern {
            ast::Pattern::Wildcard => TirPattern::Wildcard,
            ast::Pattern::Ident { id, name, span } => {
                // A bare ident in a pattern is ambiguous: a nullary
                // enum/variant case (`None`, `Red`), an immutable global
                // constant, or a fresh binding. Disambiguate in the same
                // order as `Elaborator::resolve_if_pattern_inner`
                // known case first, then immutable global, then binding.
                if let Some(case_index) = self.scrutinee_enum_case_index(scrutinee_type, name) {
                    return TirPattern::Enum {
                        enum_type: self.scrutinee_structure_head(scrutinee_type),
                        case_name: name.clone(),
                        case_index,
                    };
                }
                if self.scrutinee_has_variant_case(scrutinee_type, name) {
                    return self.reify_nullary_variant_case(scrutinee_type, name);
                }
                if let Some(const_pat) = self.reify_immutable_global_pattern(name, *span) {
                    return const_pat;
                }
                let local_index = ctx.add_local_at(
                    name.clone(),
                    scrutinee_type,
                    /* is_mut */ false,
                    Some(*id),
                    *span,
                );
                TirPattern::Binding {
                    name: name.clone(),
                    local_index,
                    type_id: scrutinee_type,
                }
            }
            ast::Pattern::MutIdent { id, name, span } => {
                let local_index = ctx.add_local_at(
                    name.clone(),
                    scrutinee_type,
                    /* is_mut */ true,
                    Some(*id),
                    *span,
                );
                TirPattern::Binding {
                    name: name.clone(),
                    local_index,
                    type_id: scrutinee_type,
                }
            }
            ast::Pattern::Literal(lit) => {
                use crate::tir::{PrimitiveType, ResolvedType, TirLiteralPattern};
                // Mirror `Elaborator::resolve_if_pattern_inner`'s literal
                // arm: wide-int literals follow the
                // scrutinee's signedness (a `u128` scrutinee must compare
                // via `u128::*`, not `i128::*`, or codegen emits a
                // `(ref $u128)` vs `(ref $i128)` mismatch), and char /
                // string literals decode their escapes. `null` on a
                // variant scrutinee with a `None` case lowers to that
                // case.
                let scrutinee_is_unsigned = {
                    let resolved = self.tysys.type_table.borrow().get(scrutinee_type).clone();
                    matches!(
                        resolved,
                        ResolvedType::Primitive(
                            PrimitiveType::U8
                                | PrimitiveType::U16
                                | PrimitiveType::U32
                                | PrimitiveType::U64
                                | PrimitiveType::U128
                        )
                    ) || matches!(resolved, ResolvedType::Struct { def, .. } if self.tysys.type_table.borrow().struct_head_name(def) == "u128")
                };
                let int_pattern = |value: u128| {
                    if scrutinee_is_unsigned {
                        TirLiteralPattern::U128(value)
                    } else {
                        TirLiteralPattern::I128(value as i128)
                    }
                };
                let tir_lit = match lit {
                    ast::Literal::Number(repr) => {
                        if scrutinee_is_unsigned {
                            int_pattern(super::util::parse_u128_literal(repr).unwrap_or(0))
                        } else {
                            TirLiteralPattern::I128(
                                super::util::parse_i128_literal(repr).unwrap_or(0),
                            )
                        }
                    }
                    ast::Literal::Byte(raw) => {
                        int_pattern(u128::from(super::util::unescape_byte(raw).unwrap_or(0)))
                    }
                    ast::Literal::Bool(b) => TirLiteralPattern::Bool(*b),
                    ast::Literal::Char(raw) => {
                        TirLiteralPattern::Char(super::util::unescape_char(raw).unwrap_or('\0'))
                    }
                    ast::Literal::String(raw) => TirLiteralPattern::String(
                        super::util::unescape_string(raw).unwrap_or_default(),
                    ),
                    ast::Literal::Null => {
                        if self.scrutinee_has_variant_case(scrutinee_type, "None") {
                            return self.reify_nullary_variant_case(scrutinee_type, "None");
                        }
                        TirLiteralPattern::Null
                    }
                    _ => ast_literal_to_pattern(lit),
                };
                TirPattern::Literal(tir_lit)
            }
            ast::Pattern::Tuple(elements, has_rest) => {
                // Tuple patterns destructure into the scrutinee's
                // element types. The elaborator already validated
                // arity; reify reads `tysys.type_table.as_tuple` to
                // get the per-element types, falling back to
                // UNKNOWN-typed inner walks for type-pack scrutinees.
                // Destructuring through a reference (`let [a, b] = &t`)
                // peels the ref for the element lookup, and each element
                // binding inherits the reference kind (match ergonomics).
                let peeled = self.tysys.type_table.borrow().peel_refs(scrutinee_type);
                let elem_types: Vec<TypeId> = self
                    .tysys
                    .type_table
                    .borrow()
                    .as_tuple(peeled)
                    .unwrap_or_default();
                let sub_patterns: Vec<TirPattern> = elements
                    .iter()
                    .enumerate()
                    .map(|(i, p)| {
                        let elem_ty = elem_types
                            .get(i)
                            .copied()
                            .unwrap_or(crate::tir::TypeTable::UNKNOWN);
                        let binding_ty = self.apply_scrutinee_ref_kind(scrutinee_type, elem_ty);
                        self.reify_pattern(p, binding_ty, ctx)
                    })
                    .collect();
                TirPattern::Tuple(sub_patterns, *has_rest)
            }
            ast::Pattern::Variant {
                variant_name,
                variant_qualifier,
                bindings,
                span,
                ..
            } => {
                // Associated-constant pattern (`TokenKind::FOO`,
                // `i32::MAX`): a nullary qualified name that resolves to a
                // recorded associated constant rather than a variant case.
                // The elaborator inlines it to the constant's value
                // reify reproduces the same lowering from
                // `sem.decls.associated_constants`. Real variant cases are
                // never in that map, so the lookup distinguishes the two.
                if bindings.is_empty()
                    && let Some(const_pat) = self.reify_associated_const_pattern(
                        variant_name,
                        variant_qualifier.as_ref(),
                        scrutinee_type,
                        *span,
                        ctx,
                    )
                {
                    return const_pat;
                }

                // Variant patterns appear in `match Some(x) { Some(v) => …
                // }` etc. The case's payload type lives on
                // `tysys.all_variant_cases`; reify reads it to give
                // sub-patterns the right scrutinee type. The
                // `variant_name` strips a `Variant::` prefix when
                // present (the AST keeps the qualified form).
                let case_name = variant_name
                    .rsplit("::")
                    .next()
                    .unwrap_or(variant_name)
                    .to_string();

                // Enum-case pattern (plain discriminant, no payload):
                // `Color::Red`. The elaborator emits `TirPattern::Enum`
                // with the case's discriminant index;
                // reify reproduces it when the scrutinee is an enum.
                if let Some(case_index) = self.scrutinee_enum_case_index(scrutinee_type, &case_name)
                {
                    return TirPattern::Enum {
                        enum_type: self.scrutinee_structure_head(scrutinee_type),
                        case_name,
                        case_index,
                    };
                }

                // Resolve the variant decl + case payload. A method that
                // takes `&self` matches on a reference (`match self { … }`
                // where `self: &Option<T>`); peel references so the
                // variant decl + payload type resolve through the
                // underlying `Option<T>` rather than falling to the
                // unknown-payload `_` arm.
                let peeled_scrutinee = self.scrutinee_structure_head(scrutinee_type);
                let payload_type = {
                    use crate::tir::ResolvedType;
                    let type_args =
                        match self.tysys.type_table.borrow().get(peeled_scrutinee).clone() {
                            ResolvedType::GenericInstance { type_args, .. } => type_args,
                            _ => Vec::<TypeId>::new(),
                        };
                    self.get_variant_case_payload_type(
                        self.tysys.type_def(peeled_scrutinee),
                        &case_name,
                        &type_args,
                    )
                };

                // Match ergonomics: a reference scrutinee (`self: &Option<T>`)
                // gives the payload binding the reference kind, so `v` is `&T`
                // and forwards to a `&self` method; `&` downgrades a `&mut`.
                // `payload_type` / `enum_type` stay peeled — only the binding
                // scrutinee carries the reference.
                let binding_scrutinee = self.apply_scrutinee_ref_kind(scrutinee_type, payload_type);
                let sub_patterns: Vec<TirPattern> = bindings
                    .iter()
                    .map(|p| self.reify_pattern(p, binding_scrutinee, ctx))
                    .collect();
                TirPattern::Variant {
                    enum_type: peeled_scrutinee,
                    variant_name: case_name,
                    bindings: sub_patterns,
                    payload_type,
                }
            }
            ast::Pattern::Or(alternatives) => {
                // Or patterns match any alternative. Each alternative
                // binds the same names, but a naive per-alternative walk
                // gives each its own local slot — so `Num(n) | Neg(n)`
                // would extract the payload into one slot and the arm body
                // read another. Mirror `resolve_if_pattern_inner`
                // remap each later alternative's binding
                // locals onto the first alternative's, then point the
                // arm-scope bindings at the first alternative's locals.
                let mut resolved: Vec<TirPattern> = Vec::with_capacity(alternatives.len());
                if let Some(first_alt) = alternatives.first() {
                    let first = self.reify_pattern(first_alt, scrutinee_type, ctx);
                    let first_bindings = super::stmt::collect_pattern_bindings_with_index(&first);
                    resolved.push(first);

                    for alt in alternatives.iter().skip(1) {
                        let alt_resolved = self.reify_pattern(alt, scrutinee_type, ctx);
                        let alt_bindings =
                            super::stmt::collect_pattern_bindings_with_index(&alt_resolved);
                        let mut remapped = alt_resolved;
                        for (first_bind, alt_bind) in first_bindings.iter().zip(alt_bindings.iter())
                        {
                            if first_bind.1 != alt_bind.1 {
                                super::stmt::remap_pattern_local(
                                    &mut remapped,
                                    alt_bind.1,
                                    first_bind.1,
                                );
                            }
                        }
                        resolved.push(remapped);
                    }

                    // Point the arm-scope bindings at the first
                    // alternative's locals so the body reads the slot the
                    // payload was extracted into.
                    for (name, local_index, _type_id) in &first_bindings {
                        if let Some(scope) = ctx.scopes.last_mut()
                            && let Some(var) = scope.get_mut(name)
                        {
                            var.index = *local_index;
                        }
                    }
                }
                TirPattern::Or(resolved)
            }
            ast::Pattern::Range {
                start, end, kind, ..
            } => {
                use crate::ast::RangeKind;
                use crate::tir::{PrimitiveType, ResolvedType};
                let inclusive = matches!(kind, RangeKind::Inclusive);
                let start_val = self.pattern_endpoint_value(start, ctx);
                let end_val = self.pattern_endpoint_value(end, ctx);
                let is_unsigned = matches!(
                    self.tysys.type_table.borrow().get(scrutinee_type),
                    ResolvedType::Primitive(
                        PrimitiveType::U8
                            | PrimitiveType::U16
                            | PrimitiveType::U32
                            | PrimitiveType::U64
                            | PrimitiveType::U128,
                    )
                );
                TirPattern::Range {
                    start: start_val,
                    end: end_val,
                    inclusive,
                    is_unsigned,
                }
            }
            ast::Pattern::Struct {
                fields, has_rest, ..
            } => self.reify_struct_pattern(fields, *has_rest, scrutinee_type, ctx),
            // `build_tir_from_state` skips reify for modules with syntax
            // errors, so reify never walks an `Error` placeholder.
            ast::Pattern::Error(_) => {
                unreachable!("reify does not run on modules with syntax errors")
            }
        }
    }

    /// Reify a struct destructuring pattern `Point { x, y }` or `{ x, y }`
    /// (anonymous). The field-name → index map comes from the scrutinee's own
    /// head; sub-patterns recurse against the declared field type. Mirrors the
    /// `Pattern::Struct` arm of the annotate-side pattern walk; shorthand `{ x }`
    /// (== `{ x: x }`) is encoded by the AST having the sub-pattern be an
    /// `Ident { name: x }` either way.
    fn reify_struct_pattern(
        &mut self,
        fields: &[ast::StructPatternField],
        has_rest: bool,
        scrutinee_type: TypeId,
        ctx: &mut FunctionContext,
    ) -> TirPattern {
        use crate::tir::{ResolvedType, TirStructPatternField};

        // Determine the struct name: explicit `Type::Pattern` wins;
        // otherwise read from the scrutinee.
        // Destructuring through a reference (`let { x, y } = &p`)
        // presents the scrutinee as `&Point`; peel references so the
        // struct decl resolves (fields inherit the reference kind below).
        let peeled_scrutinee = self.scrutinee_structure_head(scrutinee_type);

        // A field index is a fact about the value being destructured, so the
        // scrutinee's head answers and the pattern's qualifier — which annotate
        // checked against this same head — has no say. An unresolved scrutinee
        // falls back to UNKNOWN-typed sub-patterns, matching annotate.
        let scrutinee_head = match self.tysys.type_table.borrow().get(peeled_scrutinee) {
            ResolvedType::Struct { def, .. } => Some(*def),
            ResolvedType::GenericInstance { .. } => self
                .tysys
                .type_def(peeled_scrutinee)
                .map(crate::tir::StructDef::Decl),
            _ => None,
        };
        let field_info: crate::hashmap::IndexMap<String, (u32, TypeId)> = {
            let lookup = self.type_lookup();
            scrutinee_head
                .and_then(|head| lookup.struct_fields_of_head(head))
                .map(|info| {
                    info.fields
                        .iter()
                        .enumerate()
                        .map(|(i, (n, t, _))| (n.clone(), (i as u32, *t)))
                        .collect()
                })
                .unwrap_or_default()
        };

        let tir_fields: Vec<TirStructPatternField> = fields
            .iter()
            .map(|f| {
                let (field_index, field_ty) = field_info
                    .get(&f.field_name)
                    .copied()
                    .unwrap_or((0, crate::tir::TypeTable::UNKNOWN));
                // Match ergonomics: a field bound through a `&Point` /
                // `&mut Point` scrutinee is `&field` / `&mut field`.
                let binding_ty = self.apply_scrutinee_ref_kind(scrutinee_type, field_ty);
                let pattern = self.reify_pattern(&f.pattern, binding_ty, ctx);
                TirStructPatternField {
                    field_name: f.field_name.clone(),
                    field_index,
                    pattern,
                }
            })
            .collect();

        TirPattern::Struct {
            struct_type: scrutinee_type,
            fields: tir_fields,
            has_rest,
        }
    }

    /// Look up a variant case's payload type, substituted with the
    /// scrutinee's type args. Reify-side mirror of the elaborator's
    /// `Elaborator::get_variant_case_payload_type`; the lookup walks
    /// `tysys.all_variant_cases` and the local-module override map
    /// via [`TypeLookup`], so the same `TypeId` annotate produced
    /// lands on the reified pattern.
    fn get_variant_case_payload_type(
        &self,
        variant: Option<crate::defs::DefId>,
        case_name: &str,
        type_args: &[TypeId],
    ) -> TypeId {
        let (payload, type_param_indices): (TypeId, Vec<u32>) = {
            let lookup = self.type_lookup();
            let Some(variant_info) = variant.and_then(|def| lookup.variant_cases_of(def)) else {
                return crate::tir::TypeTable::UNKNOWN;
            };
            let Some(case_data) = variant_info.cases.iter().find(|c| c.name == case_name) else {
                return crate::tir::TypeTable::UNKNOWN;
            };
            // Extract the variant decl's type-param indices so the
            // substitution map below is keyed by `index` — matching
            // `TypeTable::substitute_type_params`.
            let indices: Vec<u32> = (0..variant_info.type_param_type_ids.len() as u32).collect();
            (case_data.payload, indices)
        };
        if type_args.is_empty() {
            return payload;
        }
        // Map TypeParam{index} → concrete `type_args[index]`. Recurse
        // through containers (`Ref`, `BuiltinArray`, `GenericInstance`,
        // `Function`, …) via `TypeTable::substitute_type_params`.
        let substitution: crate::hashmap::IndexMap<u32, TypeId> = type_param_indices
            .iter()
            .zip(type_args.iter())
            .map(|(&idx, &t)| (idx, t))
            .collect();
        self.tysys
            .type_table
            .borrow_mut()
            .substitute_type_params(payload, &substitution)
    }
}

/// Decode a range-pattern endpoint (`a..<b` / `a..=b`) into its
/// `i128` value. The endpoint syntactic form is itself a `Pattern`:
/// either a `Literal(Number)` or a `Literal(Char)` (for char-range
/// patterns). Char endpoints lower to their codepoint as `i128`;
/// numeric endpoints reuse the same hex / oct / bin recogniser as
/// `ast_literal_to_pattern`'s integer decode. Non-literal endpoints
/// are a parser-elaborator invariant violation — annotate has already
/// diagnosed them — so reify panics with a labelled tripwire.
fn pattern_endpoint_to_i128(endpoint: &ast::Pattern) -> i128 {
    match endpoint {
        ast::Pattern::Literal(ast::Literal::Number(repr)) => {
            let digits = repr.replace('_', "");
            if let Some(stripped) = digits.strip_prefix("0x") {
                i128::from_str_radix(stripped, 16).unwrap_or(0)
            } else if let Some(stripped) = digits.strip_prefix("0o") {
                i128::from_str_radix(stripped, 8).unwrap_or(0)
            } else if let Some(stripped) = digits.strip_prefix("0b") {
                i128::from_str_radix(stripped, 2).unwrap_or(0)
            } else {
                digits.parse::<i128>().unwrap_or(0)
            }
        }
        ast::Pattern::Literal(ast::Literal::Char(s)) => {
            let ch = super::util::unescape_char(s).unwrap_or('\0');
            i128::from(ch as u32)
        }
        ast::Pattern::Literal(ast::Literal::Byte(s)) => {
            i128::from(super::util::unescape_byte(s).unwrap_or(0))
        }
        _ => panic!(
            "pattern_endpoint_to_i128: non-literal range endpoint {endpoint:?} \
             (annotate should have diagnosed)"
        ),
    }
}

/// Map an AST [`ast::Literal`] in pattern position to its
/// [`crate::tir::TirLiteralPattern`] counterpart. Number literals
/// decode into `I128` (parsed via the same hex / oct / bin prefix
/// recogniser used by `reify_literal`), with negative sources kept
/// as their parsed numeric value. The `Null` / `Unit` literals never
/// appear in pattern position in the surface grammar — they panic
/// here to surface a parser-elaborator invariant violation early.
fn ast_literal_to_pattern(lit: &ast::Literal) -> crate::tir::TirLiteralPattern {
    use crate::tir::TirLiteralPattern;
    match lit {
        ast::Literal::Number(repr) => {
            // Mirror `reify_literal`'s numeric decode: prefer
            // hex/oct/bin radix, else decimal. Pattern position
            // never sees float literals (the elaborator rejects
            // them earlier), so decode as integer.
            let digits = repr.replace('_', "");
            let value: i128 = if let Some(stripped) = digits.strip_prefix("0x") {
                i128::from_str_radix(stripped, 16).unwrap_or(0)
            } else if let Some(stripped) = digits.strip_prefix("0o") {
                i128::from_str_radix(stripped, 8).unwrap_or(0)
            } else if let Some(stripped) = digits.strip_prefix("0b") {
                i128::from_str_radix(stripped, 2).unwrap_or(0)
            } else {
                digits.parse::<i128>().unwrap_or(0)
            };
            TirLiteralPattern::I128(value)
        }
        ast::Literal::String(s) => TirLiteralPattern::String(s.clone()),
        ast::Literal::Char(s) => {
            TirLiteralPattern::Char(super::util::unescape_char(s).unwrap_or('\0'))
        }
        ast::Literal::Byte(_) => {
            panic!("ast_literal_to_pattern: byte literal must be reified scrutinee-aware")
        }
        ast::Literal::Bool(b) => TirLiteralPattern::Bool(*b),
        ast::Literal::Null => TirLiteralPattern::Null,
        // Unit / Location / Include literals don't appear as pattern
        // literals in the surface grammar — the parser rejects them
        // earlier. Falling here would be a parser-elaborator
        // invariant violation; panic with a labelled tripwire.
        ast::Literal::Unit
        | ast::Literal::Bytes(_)
        | ast::Literal::LocationFile
        | ast::Literal::LocationLine
        | ast::Literal::LocationFunction
        | ast::Literal::DataSection
        | ast::Literal::IncludeStr(_)
        | ast::Literal::IncludeBytes(_) => {
            panic!("ast_literal_to_pattern: literal kind {lit:?} not valid in pattern position")
        }
    }
}

/// Attribute extractors, reify's own. An attribute is uniquely determined by
/// the AST alone, which is what the completeness rule lets reify re-derive, so
/// these need no recorded fact and no elaborator to run.
fn extract_is_ambient_attr(attrs: &[crate::ast::Attribute]) -> bool {
    attrs.iter().any(|a| a.name == "ambient")
}

/// Collect the effect names from every `#[benign(E, ...)]` attribute; multiple
/// attributes and arguments accumulate. The caller resolves them to
/// `EffectRef`s via `reify_effects`.
fn extract_benign_effect_names(attrs: &[crate::ast::Attribute]) -> Vec<String> {
    attrs
        .iter()
        .filter(|a| a.name == "benign")
        .flat_map(|a| a.args.iter().map(crate::ast::AttrArg::as_str))
        .map(str::to_string)
        .collect()
}

fn extract_inline_hint_attr(attrs: &[crate::ast::Attribute]) -> crate::tir::InlineHint {
    let Some(attr) = attrs.iter().find(|a| a.name == "inline") else {
        return crate::tir::InlineHint::Auto;
    };
    match attr.args.first().map(crate::ast::AttrArg::as_str) {
        Some("always") => crate::tir::InlineHint::Always,
        Some("never") => crate::tir::InlineHint::Never,
        None => crate::tir::InlineHint::Hint,
        _ => crate::tir::InlineHint::Auto,
    }
}

fn extract_export_name_attr(attrs: &[crate::ast::Attribute]) -> Option<String> {
    attrs
        .iter()
        .find(|a| a.name == "export_name")
        .and_then(|a| a.args.first())
        .map(|a| a.as_str().to_string())
}

fn extract_allocator_tag_attr(attrs: &[crate::ast::Attribute]) -> Option<String> {
    attrs
        .iter()
        .find(|a| a.name == "allocator")
        .and_then(|a| a.args.first())
        .map(|a| a.as_str().to_string())
}

/// True when `arg` is a closure literal with at least one param that lacks a
/// type annotation. Reify forwards the recorded callee param type as the
/// closure's expected type only in this case: it is what lets an unannotated
/// `|a, b| ...` infer its params from a `fn`-typed (or `fn`-newtype) param.
/// Closures whose params are fully annotated (or take no params) gain nothing
/// and must not receive the expected type — doing so would pin an
/// effect-polymorphic closure's `declared_effects` to a generic effect param
/// instead of the effects inferred from its body.
fn arg_is_unannotated_closure(arg: &ast::Expr) -> bool {
    matches!(arg, ast::Expr::Closure(c) if c.params.iter().any(|p| p.ty.is_none()))
}

/// Compile-time value and primitive `TypeId` for a primitive integer
/// associated constant named `<prefix>::<suffix>` (e.g. `i32::MAX`).
/// Returns `None` for non-primitive or unknown constants. Used by
/// `reify_ident` to resolve such constants when they are not present in
/// `associated_constants` — e.g. a default-argument expression reified
/// under a stdlib-snapshot callee module whose `associated_constants` map
/// was not rehydrated. The value table mirrors
/// [`super::stmt::primitive_assoc_const_to_i128`].
fn primitive_int_assoc_const(prefix: &str, suffix: &str) -> Option<(i128, crate::tir::TypeId)> {
    use crate::tir::TypeTable;
    let ty = match prefix {
        "i8" => TypeTable::I8,
        "i16" => TypeTable::I16,
        "i32" => TypeTable::I32,
        "i64" => TypeTable::I64,
        "u8" => TypeTable::U8,
        "u16" => TypeTable::U16,
        "u32" => TypeTable::U32,
        "u64" => TypeTable::U64,
        _ => return None,
    };
    let value = match (prefix, suffix) {
        ("i8", "MAX") => i128::from(i8::MAX),
        ("i8", "MIN") => i128::from(i8::MIN),
        ("i16", "MAX") => i128::from(i16::MAX),
        ("i16", "MIN") => i128::from(i16::MIN),
        ("i32", "MAX") => i128::from(i32::MAX),
        ("i32", "MIN") => i128::from(i32::MIN),
        ("i64", "MAX") => i128::from(i64::MAX),
        ("i64", "MIN") => i128::from(i64::MIN),
        ("u8", "MAX") => i128::from(u8::MAX),
        ("u8", "MIN") => i128::from(u8::MIN),
        ("u16", "MAX") => i128::from(u16::MAX),
        ("u16", "MIN") => i128::from(u16::MIN),
        ("u32", "MAX") => i128::from(u32::MAX),
        ("u32", "MIN") => i128::from(u32::MIN),
        ("u64", "MAX") => i128::from(u64::MAX),
        ("u64", "MIN") => i128::from(u64::MIN),
        _ => return None,
    };
    Some((value, ty))
}

/// Build the receiver node the recorded `(self_kind, is_ref_impl)` pair asks
/// for. The type it lands on is the body walk's own answer
/// ([`super::method_lookup::adjusted_receiver_type`]), asserted here so the
/// node builder and the rule it implements cannot drift apart silently.
fn adjust_receiver_for_self_kind(
    receiver: TirExpr,
    self_kind: ast::SelfKind,
    is_ref_impl: bool,
    span: crate::token::Span,
    type_table: &std::cell::RefCell<crate::tir::TypeTable>,
) -> TirExpr {
    let expected = super::method_lookup::adjusted_receiver_type(
        receiver.type_id,
        self_kind,
        is_ref_impl,
        type_table,
    );
    let adjusted = adjust_receiver_node(receiver, self_kind, is_ref_impl, span, type_table);
    assert_eq!(
        adjusted.type_id, expected,
        "the receiver node reify built disagrees with the adjusted type annotate recorded"
    );
    adjusted
}

fn adjust_receiver_node(
    receiver: TirExpr,
    self_kind: ast::SelfKind,
    is_ref_impl: bool,
    span: crate::token::Span,
    type_table: &std::cell::RefCell<crate::tir::TypeTable>,
) -> TirExpr {
    if is_ref_impl {
        // For ref-type impls, Self is &T (or &mut T).
        // &self means &&T, &mut self means &mut &T.
        // The receiver is already &T, so we need to add an extra reference layer.
        return match self_kind {
            ast::SelfKind::Ref => {
                let ref_type = type_table.borrow_mut().make_ref(receiver.type_id);
                TirExpr::new(
                    TirExprKind::Unary {
                        op: TirUnaryOp::Ref,
                        expr: Box::new(receiver),
                    },
                    ref_type,
                    span,
                )
            }
            ast::SelfKind::MutRef => {
                let mut_ref_type = type_table.borrow_mut().make_mut_ref(receiver.type_id);
                TirExpr::new(
                    TirExprKind::Unary {
                        op: TirUnaryOp::MutRef,
                        expr: Box::new(receiver),
                    },
                    mut_ref_type,
                    span,
                )
            }
            ast::SelfKind::None | ast::SelfKind::Value => {
                deref_to_value(receiver, span, type_table)
            }
        };
    }

    let receiver_type = type_table.borrow().get(receiver.type_id).clone();

    match self_kind {
        ast::SelfKind::None | ast::SelfKind::Value => {
            // No auto-ref: static method context, or a by-value `self`
            // receiver that transfers the resource. Deref all refs.
            deref_to_value(receiver, span, type_table)
        }
        ast::SelfKind::Ref => {
            // Method expects &self
            match &receiver_type {
                ResolvedType::Ref(_) => {
                    // Already &T, use as-is
                    receiver
                }
                ResolvedType::MutRef(_) => {
                    // &mut T can be coerced to &T, use as-is
                    receiver
                }
                _ => {
                    // Value T, need to add &
                    let ref_type = type_table.borrow_mut().make_ref(receiver.type_id);
                    TirExpr::new(
                        TirExprKind::Unary {
                            op: TirUnaryOp::Ref,
                            expr: Box::new(receiver),
                        },
                        ref_type,
                        span,
                    )
                }
            }
        }
        ast::SelfKind::MutRef => {
            // Method expects &mut self
            if let ResolvedType::MutRef(_) = &receiver_type {
                // Already &mut T, use as-is
                receiver
            } else {
                // Value T, need to add &mut
                let mut_ref_type = type_table.borrow_mut().make_mut_ref(receiver.type_id);
                TirExpr::new(
                    TirExprKind::Unary {
                        op: TirUnaryOp::MutRef,
                        expr: Box::new(receiver),
                    },
                    mut_ref_type,
                    span,
                )
            }
        }
    }
}

/// Peel every reference layer off a receiver, so a by-value `self` reaches the
/// callee as the value it declares.
fn deref_to_value(
    mut receiver: TirExpr,
    span: crate::token::Span,
    type_table: &std::cell::RefCell<crate::tir::TypeTable>,
) -> TirExpr {
    loop {
        match type_table.borrow().get(receiver.type_id).clone() {
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
                receiver = TirExpr::new(
                    TirExprKind::Unary {
                        op: TirUnaryOp::Deref,
                        expr: Box::new(receiver),
                    },
                    inner,
                    span,
                );
            }
            _ => return receiver,
        }
    }
}

/// Build the `from_pair` call that materializes a 128-bit value from its
/// `(low: u64, high: u64/i64)` halves.
fn build_int128_from_pair(
    type_name: &FqTypeName,
    low: u64,
    high: i64,
    target_type: TypeId,
    span: crate::token::Span,
) -> TirExpr {
    let low_literal = TirExpr::new(
        TirExprKind::IntLiteral {
            value: low,
            repr: low.to_string(),
        },
        TypeTable::U64,
        span,
    );
    let high_literal = TirExpr::new(
        TirExprKind::IntLiteral {
            value: high.cast_unsigned(),
            repr: high.to_string(),
        },
        if type_name.decl_name() == "u128" {
            TypeTable::U64
        } else {
            TypeTable::I64
        },
        span,
    );

    let method_info =
        crate::name::LocalMethodName::new(type_name.clone(), None, "from_pair".to_string());
    let mangled_func_name = method_info.to_mangled_name();

    TirExpr::new(
        TirExprKind::Call {
            func: Box::new(crate::tir::FunctionRef {
                module_source: ModuleSource::int128(),
                name: mangled_func_name,
                monomorph_info: None,
                method_info: Some(method_info),
            }),
            type_args: vec![],
            args: vec![
                CallArg::new(low_literal, false),
                CallArg::new(high_literal, false),
            ],
            has_receiver: false,
        },
        target_type,
        span,
    )
}

/// Materialize an `i128` / `u128` from a parsed numeric literal. `allow_small`
/// admits the cheaper `from_u64` / `from_i64`; the negated `-NUM` shape denies
/// it and always takes `from_pair`.
fn build_int128_literal_call(
    name: &FqTypeName,
    value: i128,
    repr: &str,
    allow_small: bool,
    target_type: TypeId,
    span: crate::token::Span,
) -> TirExpr {
    let use_small = allow_small
        && if name.decl_name() == "u128" {
            u64::try_from(value).is_ok()
        } else {
            i64::try_from(value).is_ok()
        };

    if use_small {
        let (inner_type, method_name, store_value) = if name.decl_name() == "u128" {
            (
                TypeTable::U64,
                "from_u64",
                u64::try_from(value).expect("value fits in u64"),
            )
        } else {
            (
                TypeTable::I64,
                "from_i64",
                i64::try_from(value)
                    .expect("value fits in i64")
                    .cast_unsigned(),
            )
        };

        let inner_literal = TirExpr::new(
            TirExprKind::IntLiteral {
                value: store_value,
                repr: repr.to_string(),
            },
            inner_type,
            span,
        );

        let method_info =
            crate::name::LocalMethodName::new(name.clone(), None, method_name.to_string());
        let mangled_func_name = method_info.to_mangled_name();

        return TirExpr::new(
            TirExprKind::Call {
                func: Box::new(crate::tir::FunctionRef {
                    module_source: ModuleSource::int128(),
                    name: mangled_func_name,
                    monomorph_info: None,
                    method_info: Some(method_info),
                }),
                type_args: vec![],
                args: vec![CallArg::new(inner_literal, false)],
                has_receiver: false,
            },
            target_type,
            span,
        );
    }

    let (low, high) = super::util::unpack_i128(value);
    build_int128_from_pair(name, low, high, target_type, span)
}

/// Build `u128::from_u64(inner)` / `i128::from_i64(inner)` for the general
/// (non-literal) `expr as i128/u128` cast. `intermediate` is already `u64`/`i64`.
fn build_int128_from_intermediate(
    name: &FqTypeName,
    intermediate: TirExpr,
    target_type: TypeId,
    span: crate::token::Span,
) -> TirExpr {
    let method_name = if name.decl_name() == "u128" {
        "from_u64"
    } else {
        "from_i64"
    };
    let method_info =
        crate::name::LocalMethodName::new(name.clone(), None, method_name.to_string());
    let mangled_func_name = method_info.to_mangled_name();
    TirExpr::new(
        TirExprKind::Call {
            func: Box::new(crate::tir::FunctionRef {
                module_source: ModuleSource::int128(),
                name: mangled_func_name,
                monomorph_info: None,
                method_info: Some(method_info),
            }),
            type_args: vec![],
            args: vec![CallArg::new(intermediate, false)],
            has_receiver: false,
        },
        target_type,
        span,
    )
}

/// Wrap an `Ord::cmp` call into a `bool` by comparing the returned
/// `Ordering` variant against the one that makes the operator true:
/// `<` → `cmp == Less`, `>` → `cmp == Greater`,
/// `<=` → `cmp != Greater`, `>=` → `cmp != Less`.
fn ord_bool_from_cmp(
    cmp_call: TirExpr,
    op: ast::BinaryOp,
    span: crate::token::Span,
    type_table: &std::cell::RefCell<crate::tir::TypeTable>,
) -> TirExpr {
    let ordering_type_id = type_table
        .borrow_mut()
        .make_compiler_enum(crate::compiler_item::CompilerItem::Ordering);
    // Look up Ordering's `Less` / `Greater` cases through the
    // `CompilerItem` registry so a stdlib rename of either case
    // flows here without touching the operator-lowering path.
    let (less_name, less_index, greater_name, greater_index) = {
        let tt = type_table.borrow();
        let items = tt.compiler_items();
        let (_, _, less_name, less_index) =
            items.require_enum_case(crate::compiler_item::CompilerItem::OrderingLess);
        let (_, _, greater_name, greater_index) =
            items.require_enum_case(crate::compiler_item::CompilerItem::OrderingGreater);
        (
            less_name.to_string(),
            less_index,
            greater_name.to_string(),
            greater_index,
        )
    };
    use crate::tir::TirBinaryOp;
    let (compare_op, case_name, case_index): (TirBinaryOp, String, u32) = match op {
        ast::BinaryOp::Lt => (TirBinaryOp::Eq, less_name, less_index),
        ast::BinaryOp::Gt => (TirBinaryOp::Eq, greater_name, greater_index),
        ast::BinaryOp::LtEq => (TirBinaryOp::NotEq, greater_name, greater_index),
        ast::BinaryOp::GtEq => (TirBinaryOp::NotEq, less_name, less_index),
        _ => unreachable!(),
    };
    let ordering_variant = TirExpr::new(
        crate::tir::TirExprKind::EnumConstruct {
            enum_type: ordering_type_id,
            case_name,
            case_index,
        },
        ordering_type_id,
        span,
    );
    // The callee is `Ord::cmp` by construction, so it returns `Ordering`
    // whatever the dispatch recorded — a trait-bounded receiver leaves that
    // unresolved, and the comparison below would have no type to lower from.
    let mut cmp_call = cmp_call;
    cmp_call.type_id = ordering_type_id;
    TirExpr::new(
        crate::tir::TirExprKind::Binary {
            op: compare_op,
            left: Box::new(cmp_call),
            right: Box::new(ordering_variant),
        },
        crate::tir::TypeTable::BOOL,
        span,
    )
}

/// The sole constructor of a method call — a [`crate::tir::TirExprKind::Call`]
/// whose receiver heads its `args`. Centralising it gives one audit point for
/// "every emitted method call was typechecked against the callee's declared
/// parameter types", though the typechecking itself is annotate's job.
fn build_tir_method_call(
    receiver: TirExpr,
    func: crate::tir::FunctionRef,
    type_args: Vec<TypeId>,
    args: Vec<crate::tir::CallArg>,
    return_type: TypeId,
    span: crate::token::Span,
) -> TirExpr {
    TirExpr::new(
        crate::tir::TirExprKind::method_call(Box::new(receiver), func, type_args, args),
        return_type,
        span,
    )
}

fn ast_unary_op_to_tir(op: ast::UnaryOp) -> crate::tir::TirUnaryOp {
    use crate::tir::TirUnaryOp;
    match op {
        ast::UnaryOp::Neg => TirUnaryOp::Neg,
        ast::UnaryOp::Not => TirUnaryOp::Not,
        ast::UnaryOp::BitNot => TirUnaryOp::BitNot,
        ast::UnaryOp::Ref => TirUnaryOp::Ref,
        ast::UnaryOp::MutRef => TirUnaryOp::MutRef,
        ast::UnaryOp::Deref => TirUnaryOp::Deref,
    }
}

/// Map an AST [`ast::BinaryOp`] to its TIR counterpart. The mapping is
/// 1:1 for the source-level ops; TIR adds `RefEq` / `RefNotEq` as
/// internal variants that the elaborator only synthesises after
/// coercion analysis, so reify never produces them from this helper.
fn ast_binary_op_to_tir(op: ast::BinaryOp) -> crate::tir::TirBinaryOp {
    use crate::tir::TirBinaryOp;
    match op {
        ast::BinaryOp::Add => TirBinaryOp::Add,
        ast::BinaryOp::Sub => TirBinaryOp::Sub,
        ast::BinaryOp::Mul => TirBinaryOp::Mul,
        ast::BinaryOp::Div => TirBinaryOp::Div,
        ast::BinaryOp::Mod => TirBinaryOp::Mod,
        ast::BinaryOp::Eq => TirBinaryOp::Eq,
        ast::BinaryOp::NotEq => TirBinaryOp::NotEq,
        ast::BinaryOp::Lt => TirBinaryOp::Lt,
        ast::BinaryOp::LtEq => TirBinaryOp::LtEq,
        ast::BinaryOp::Gt => TirBinaryOp::Gt,
        ast::BinaryOp::GtEq => TirBinaryOp::GtEq,
        ast::BinaryOp::And => TirBinaryOp::And,
        ast::BinaryOp::Or => TirBinaryOp::Or,
        ast::BinaryOp::BitAnd => TirBinaryOp::BitAnd,
        ast::BinaryOp::BitOr => TirBinaryOp::BitOr,
        ast::BinaryOp::BitXor => TirBinaryOp::BitXor,
        ast::BinaryOp::Shl => TirBinaryOp::Shl,
        ast::BinaryOp::Shr => TirBinaryOp::Shr,
    }
}

/// `#[wire(name = "...")]` on a struct field, enum case, or variant case.
fn wire_name_override_of(attrs: &[ast::Attribute]) -> Option<String> {
    attrs.iter().find_map(|a| {
        if a.name == "wire" {
            a.kv_value("name").map(str::to_string)
        } else {
            None
        }
    })
}

/// `#[wire(name_policy = "...")]` on a struct, enum, or variant declaration.
fn wire_name_policy_of(attrs: &[ast::Attribute]) -> Option<String> {
    attrs.iter().find_map(|a| {
        if a.name == "wire" {
            a.kv_value("name_policy").map(str::to_string)
        } else {
            None
        }
    })
}

/// First `return <value>` type reachable in a reified closure body.
///
/// TIR counterpart of `control_flow::find_return_type_in_block`, and must stay
/// in step with it: a construct missing here is a `return` the closure's return
/// type cannot see, which mistypes `__call` and fails core-Wasm validation.
fn tir_block_return_type(body: &crate::tir::TirExpr) -> Option<crate::tir::TypeId> {
    use crate::tir::{TirExprKind, TirStmtKind};

    fn in_block(block: &crate::tir::TirBlock) -> Option<crate::tir::TypeId> {
        block.stmts.iter().find_map(|stmt| match &stmt.kind {
            TirStmtKind::Return { value } => value.as_ref().map(|v| v.type_id),
            TirStmtKind::If {
                then_block,
                else_block,
                ..
            } => in_block(then_block).or_else(|| else_block.as_ref().and_then(in_block)),
            TirStmtKind::Loop { body, .. } => in_block(body),
            TirStmtKind::LabeledBlock { block, .. } => in_block(block),
            TirStmtKind::VariadicForOf { body, .. } => in_block(body),
            TirStmtKind::Let { value, .. } => in_expr(value),
            TirStmtKind::Expr(e) => in_expr(e),
            _ => None,
        })
    }

    fn in_expr(expr: &crate::tir::TirExpr) -> Option<crate::tir::TypeId> {
        match &expr.kind {
            TirExprKind::Block(block) => in_block(block),
            TirExprKind::If {
                then_branch,
                else_branch,
                ..
            } => in_block(then_branch).or_else(|| else_branch.as_ref().and_then(in_block)),
            TirExprKind::Match { arms, .. } => arms.iter().find_map(|arm| in_expr(&arm.body)),
            TirExprKind::WithHandler { body, .. } => in_block(body),
            TirExprKind::Resume { value } => Some(value.type_id),
            _ => None,
        }
    }

    in_expr(body)
}

/// The operations an interface declares a default implementation for, each
/// renamed to its synthesized function name.
///
/// The rename is what keeps a default out of the module's own namespace: an
/// operation and a facade function that wraps it share a name by design
/// (`core:log`'s `Log::event` and `event`), and every fact either pass records
/// is keyed by the unchanged `AstId`, so resolve and reify still agree.
pub(crate) fn default_impl_methods(decl: &crate::ast::InterfaceDecl) -> Vec<crate::ast::Function> {
    decl.methods
        .iter()
        .filter(|method| method.body.is_some())
        .map(|method| crate::ast::Function {
            name: crate::name::effect_default_impl_name(&decl.name, &method.name),
            ..method.clone()
        })
        .collect()
}
