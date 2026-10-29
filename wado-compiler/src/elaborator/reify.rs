//! Reify — AST + [`super::sem::ModuleSemantics`] → [`crate::tir::TirModule`].
//! The mechanical half of the annotate/reify split (WEP 2026-05-26): every
//! TIR-shaping decision is already recorded on `ModuleSemantics`, so this walker
//! only reads them — never re-running inference, resolution, or dispatch.

use super::sig::AssocConstSig;
use std::ops::ControlFlow;
use std::rc::Rc;

use crate::ast::{
    self, AstId, AstVisitor, CompoundAssignOp, Expr, Item, Module, UnaryOp, walk_expr,
};
use crate::attribute::{
    self, ALLOC, AMBIENT, CM, EXPORT_NAME, IMMEDIATE, INLINE, PARAM, SECRET, SIDE_EFFECT, STORAGE,
    WIRE,
};
use crate::builtin_facts::{self, BuiltinFacts, ParamShape, ReturnShape};
use crate::call_args::CallArgs;
use crate::compiler_host::{Code, CompilerHost, Diagnostic, DiagnosticSpan, Severity};
use crate::const_eval::{format_float_repr, truncate_int};
use crate::hashmap::{IndexMap, IndexSet};
use crate::logger::{Bail, Logger};
use crate::lower::contract_checks::is_contract_clause;
use crate::lower::plan::value_copy::place::{is_source_place, source_place_subscripts_mut};
use crate::lower::wide_int_literal::{create_conversion, create_literal, method_ref};
use crate::module_source::ModuleSource;
use crate::name::{
    CONTRACT_CLAUSE_INTRINSIC, CONTRACT_SITE_INTRINSIC, FqTypeName, Receiver, contract_owner_name,
    global_init_function, global_name, unchecked_twin_name,
};
use crate::ownership::{self, BodyFacts as _, Modes};
use crate::symbol::SymbolTable;
use crate::tir::{
    self as tir, CallArg, GlobalInit, InstancePattern, LetStorage, LocalFrame, ResolvedType,
    TirBinaryOp, TirBlock, TirEnum, TirEnumCase, TirExpr, TirExprKind, TirFlags, TirFlagsMember,
    TirFunction, TirGlobal, TirModule, TirNewtype, TirPattern, TirStmt, TirStmtKind, TirStruct,
    TirTest, TirUnaryOp, TirVariantDecl, TypeId, assign_stmt, transpose_tuple_expr,
};
use crate::type_table::TypeTable;

use super::coercion::{
    NumericLiteralKind, classify_numeric_literal, is_literal_arithmetic,
    is_primitive_literal_target, numeric_literal_pair_order, range_endpoint_order,
    unary_passes_expected_type,
};
use super::expr::UnionSource;
use super::sem::{ModuleSemantics, TypeAnnotations};
use super::transposition::Transposition;
use super::typecheck::admits_condition;
use super::types::{FunctionContext, TypeLookup, VariantInfo};
use super::tysys::TypeSystem;
use super::util;
use crate::ast::RangeKind;
use crate::ast::{AttrArg, Attribute, InterfaceDecl, NamePolicy, Visibility};
use crate::compiler_item::{CompilerItem, Resolved};
use crate::coverage::{CoverageMap, ProbeSite};
use crate::defs::DefId;
use crate::elaborator::Elaborator;
use crate::elaborator::assert::{MOVED, NOT_EVALUATED, render_local_name, seen_local_name};
use crate::elaborator::call::{ARRAY_NEW_DATA, RECEIVER, omits_a_default};
use crate::elaborator::closure::relink_recorded_captures;
use crate::elaborator::control_flow::{CtrlFlowCtx, find_return_type_in_block};
use crate::elaborator::expr::{compose_union_plan, peel_to_struct};
use crate::elaborator::float_literal::{FloatFormat, float_literal_bits};
use crate::elaborator::item::extract_compiler_item;
use crate::elaborator::method_lookup::adjusted_receiver_type;
use crate::elaborator::sem::types::{
    BodyFacts, CoercionKind, DesugarKind, ForOfIteratorInfo, FromCallFacts, ImplFacts,
    IndirectCallee, KeyValueCoercionFacts, LiteralCallee, LiteralFromCall, MethodDispatch,
    MethodNames, OperatorDispatch, SequenceCoercionFacts, StaticMethodDispatch, with_body_facts,
};
use crate::elaborator::stmt::{
    RefBinding, collect_pattern_bindings_with_index, remap_pattern_local,
};
use crate::elaborator::trait_query::trait_sig_of_with;
use crate::elaborator::types::{VarRef, newtype_member_owner};
use crate::elaborator::unchecked::Contracts;
use crate::elaborator::util::{
    BoundValue, FloatRange, PatternLiteralError, SettledPattern, bound_value,
    constant_pattern_site, float_limit_pattern, literal_pattern, parse_i128_literal,
    parse_u128_literal, range_bound, range_pattern, settle_instance_pattern,
    settles_literal_patterns,
};
use crate::escape::{
    unescape_byte, unescape_bytes, unescape_char, unescape_string, unescape_template_segment,
};
use crate::format_spec::FormatKind;
use crate::hashmap;
use crate::lexer::defaults_to_float;
use crate::name::{
    DeclaredHead, LocalMethodName, MethodName, constant_pattern_local_name, deref_capture_name,
    display_function_name, effect_default_impl_name, for_body_label, mangle_local_item_name,
    minted_name, range_local_name, template_block_label, template_hole_local, test_function_name,
    widened_half_local_name,
};
use crate::pgo::ColdMap;
use crate::primitive::PrimitiveType;
use crate::symbol::{Symbol, SymbolKind};
use crate::synthesis::common::{bool_lit, handle_bits, handle_from_f64, handle_to_f64, not_expr};
use crate::synthesis::common::{
    builtin_call, cold_path_stmt, is_cold_marker_stmt, known_cold_path_stmt,
};
use crate::tir::{
    EffectRef, StructDef, TemplateId, TirEffectOp, TirField, TirImpl, TirParam, TirTypeParam,
    agree_branch_types,
};
use crate::tir::{
    FunctionKind, FunctionRef, InlineHint, MonomorphInfo, ReturnAbi, TemplateHole, TemplateShape,
    TirHandlerBinding, TirLiteralPattern, TirMatchArm, TirStructField, TirStructPatternField,
    deref_to_layers, hold_hole, to_layers_of, wrapped_reference,
};
use crate::tir_visitor::TirMutVisitor;
use crate::token::Span;
use crate::unparse::{unparse_expr_source, unparse_instance_pattern, unparse_pattern_source};

/// A template's text and holes as reify assembles it: one segment more than
/// holes, each segment cooked and raw, and each hole with its specifier as
/// written.
struct TemplatePieces {
    segments: Vec<[String; 2]>,
    holes: Vec<(TirExpr, Option<String>)>,
}

impl Default for TemplatePieces {
    fn default() -> Self {
        Self {
            segments: vec![Default::default()],
            holes: Vec::new(),
        }
    }
}

/// The statements binding the holes a template literal cannot read where it
/// stands, and the serial naming them.
struct HoleBindings {
    lets: Vec<TirStmt>,
    serial: u32,
}

impl TemplatePieces {
    fn text(&mut self, cooked: &str, raw: &str) {
        let [c, r] = self.segments.last_mut().expect("starts with one segment");
        c.push_str(cooked);
        r.push_str(raw);
    }

    /// Text that no source spelled, so it has no raw form of its own.
    fn literal(&mut self, text: &str) {
        self.text(text, text);
    }

    fn hole(&mut self, value: TirExpr, spec: Option<String>) {
        self.holes.push((value, spec));
        self.segments.push(Default::default());
    }
}

/// Generate the `ann_*` annotation accessors on [`Reify`], one per
/// [`super::sem::types::BodyFacts`] map from the list `with_body_facts!`
/// names, each reading through [`Reify::ann`]. See the accessor doc comment
/// on the `impl` block for why this exists.
macro_rules! reify_annotation_accessors {
    ($($(#[$doc:meta])* $name:ident => $map:ident : $val:ty),+ $(,)?) => {
        $(
            $(#[$doc])*
            fn $name(&self, id: AstId) -> Option<$val> {
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
            fn $name(&self, id: AstId) -> Option<$val> {
                self.types.$map.get(&id).cloned()
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
/// materialises a `$b` builder local under sequence coercion.
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
/// reify (binds `let $ca_N`, overrides) and annotate (reserves the matching
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

/// Where an assignment writes, reified ahead of the value it takes.
enum WritePlace {
    /// `recv[i] = v`, which becomes `recv.index_assign(i, v)`.
    IndexAssign {
        receiver: TirExpr,
        index: Box<TirExpr>,
        dispatch: Box<OperatorDispatch>,
    },
    /// Any other place; a global's becomes a `GlobalVarSet`.
    Place(TirExpr),
}

impl WritePlace {
    fn value_type(&self) -> Option<TypeId> {
        match self {
            Self::IndexAssign { .. } => None,
            Self::Place(target) => Some(target.type_id),
        }
    }

    fn write(self, value: TirExpr, span: Span) -> TirExpr {
        let target = match self {
            Self::IndexAssign {
                receiver,
                index,
                dispatch,
            } => {
                return build_tir_method_call(
                    receiver,
                    dispatch.function_ref,
                    vec![],
                    vec![CallArg::new(*index, false), CallArg::new(value, false)],
                    dispatch.return_type,
                    span,
                );
            }
            Self::Place(target) => target,
        };
        let kind = match target.kind {
            TirExprKind::GlobalVarGet {
                module_source,
                name,
            } => TirExprKind::GlobalVarSet {
                module_source,
                name,
                value: Box::new(value),
            },
            _ => TirExprKind::Assign {
                target: Box::new(target),
                value: Box::new(value),
            },
        };
        TirExpr::new(kind, TypeTable::UNIT, span)
    }
}

/// One reify-side power-assert capture slot. Independent from
/// [`super::assert::Capture`] so the two walks don't share state.
pub(super) struct ReifyAssertSlot {
    /// `$v0`, `$v1`, … — the local the panic template references.
    pub(super) name: String,
    pub(super) label: String,
    /// `false` when the sub-expression evaporated during reify and no
    /// binding was emitted; the template skips the slot.
    pub(super) emitted: bool,
    pub(super) local_index: Option<u32>,
    pub(super) type_id: Option<tir::TypeId>,
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
fn literal_callee_ref(callee: &LiteralCallee) -> tir::FunctionRef {
    let method_info = callee.method_info();
    FunctionRef {
        module_source: callee.impl_module_source.clone(),
        name: method_info.to_mangled_name(),
        template: callee.method_def.map(|def| TemplateId::Declared {
            def,
            block: callee.impl_def,
        }),
        monomorph_info: (!callee.type_arg_ids.is_empty()).then(|| MonomorphInfo {
            generic_name: MethodName::format_local(&callee.target_base_name, None, callee.method),
            impl_type_args: callee.type_arg_ids.clone(),
            method_type_args: vec![],
            is_blanket: false,
        }),
        method_info: Some(method_info),
    }
}

/// `Output::from(array)` for a literal coercion (WEP 2026-08-24). An `Array<E>`
/// target needs no conversion — the array the literal materializes is already
/// the result — and is returned unchanged.
fn build_literal_from_call(array: TirExpr, call: &LiteralFromCall, span: Span) -> TirExpr {
    if call.from_type == call.output_type {
        return array;
    }
    TirExpr::new(
        TirExprKind::Call {
            func: Box::new(literal_callee_ref(&call.callee)),
            type_args: vec![],
            args: CallArgs::free(vec![CallArg::new(array, false)]),
        },
        call.output_type,
        span,
    )
}

/// `operand` dereferenced through what a cast to `target` reads through.
fn read_through_for_cast(tt: &TypeTable, operand: TirExpr, target: tir::TypeId) -> TirExpr {
    tt.cast_read_through(operand.type_id, target)
        .into_iter()
        .fold(operand, |operand, referent| {
            let span = operand.span;
            TirExpr::new(
                TirExprKind::Unary {
                    op: TirUnaryOp::Deref,
                    expr: Box::new(operand),
                },
                referent,
                span,
            )
        })
}

/// Cast a `from` result to the newtype the literal targeted, where it targeted
/// one.
fn cast_to_newtype(built: TirExpr, newtype_cast_to: Option<tir::TypeId>, span: Span) -> TirExpr {
    match newtype_cast_to {
        Some(target_type) => bare_cast(built, target_type, span),
        None => built,
    }
}

/// `Output::from([[k0, v0], …])` over one run of a key-value literal's explicit
/// members.
fn build_kv_from_call(pairs: Vec<TirExpr>, facts: &KeyValueCoercionFacts, span: Span) -> TirExpr {
    let array = TirExpr::new(
        TirExprKind::ArrayLiteral { elements: pairs },
        facts.call.from_type,
        span,
    );
    build_literal_from_call(array, &facts.call, span)
}

/// `$acc.spread_literal(base)` for one `..base` member.
fn build_literal_spread_call(
    acc_index: u32,
    output_type: tir::TypeId,
    base: TirExpr,
    spread: &LiteralCallee,
    span: Span,
) -> TirExpr {
    let receiver = TirExpr::new(
        TirExprKind::Local {
            index: acc_index,
            name: "$acc".to_string(),
        },
        output_type,
        span,
    );
    TirExpr::new(
        TirExprKind::method_call(
            Box::new(receiver),
            literal_callee_ref(spread),
            vec![],
            vec![CallArg::new(base, false)],
        ),
        TypeTable::UNIT,
        span,
    )
}

/// `break;`, leaving the innermost loop.
fn bare_break(span: Span) -> TirStmt {
    TirStmt::new(
        TirStmtKind::Break {
            label: None,
            value: None,
        },
        span,
    )
}

/// `if !cond { break; }`, a loop's exit test.
fn break_unless(cond: TirExpr, cond_span: Span, span: Span) -> TirStmt {
    let neg_cond = not_expr(cond, cond_span);
    TirStmt::new(
        TirStmtKind::If {
            condition: neg_cond,
            then_block: TirBlock::new(vec![bare_break(span)], span),
            else_block: None,
        },
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
    /// The module reify stands in: its declarations and imports, which name
    /// resolution reads. Read only — reify never mutates the recorded decisions.
    pub(crate) sem: &'a ModuleSemantics,
    /// The per-node facts of the walk reify replays: `sem.types`, except
    /// inside a trait default method, where they are the impl's own record.
    pub(crate) types: &'a TypeAnnotations,
    /// All modules' semantics, keyed by source. Used to swap `sem` to a
    /// callee module when reifying a default-argument expression that
    /// resolves in the callee's lexical scope (it may reference items
    /// private to the callee module).
    pub(crate) all_module_semantics: &'a IndexMap<ModuleSource, Rc<ModuleSemantics>>,
    /// Symbol table from analyzer (cross-module).
    pub(crate) symbols: &'a SymbolTable,
    /// All loaded modules. Used by cross-module lookups (e.g. resolving
    /// the AST of a function referenced by a `FunctionRef`).
    pub(crate) loaded_modules: &'a IndexMap<ModuleSource, Module>,
    /// Diagnostics logger.
    pub(crate) logger: &'a Logger<'a, H>,
    /// Source module currently being reified.
    pub(crate) current_module_source: ModuleSource,
    /// The module whose TIR is being built, which `current_module_source`
    /// leaves while code is reified standing in another (`with_perspective`).
    pub(crate) emitted_module_source: ModuleSource,
    /// Items of the current module, set before per-Item dispatch.
    pub(crate) current_module_items: &'a [Item],

    /// Active per-element annotation overlays for the tuple `for-of`(s)
    /// currently being unrolled, innermost last. While reifying element
    /// `i` of a tuple for-of, that element's [`super::sem::types::BodyFacts`]
    /// sits on top; [`Self::ann`] consults the stack from the top down before
    /// falling back to `sem.types`. A nested inner for-of pushes its own
    /// overlay above the outer one, so inner-body nodes shadow correctly while
    /// outer-body nodes fall through to the outer overlay. See
    /// [`Self::reify_tuple_for_of`].
    pub(crate) tuple_overlay_stack: Vec<&'a BodyFacts>,
    /// Per-`ForOfStmt` visit counter. Annotate records one overlay set per
    /// *instantiation* of a tuple for-of in walk order; reify increments
    /// this each time it reifies the same `for_of.id` so it consumes the
    /// matching instantiation (a nested inner for-of is instantiated once
    /// per outer element). See [`Self::reify_tuple_for_of`].
    pub(crate) tuple_overlay_visits: IndexMap<AstId, usize>,
    /// Source-level emit set. When `Some`, reify skips a function or method
    /// whose `AstId` is absent — one the liveness pass found the emitted
    /// program cannot reach, which downstream phases would discard anyway.
    /// `None` reifies everything.
    pub(crate) emit_live: Option<&'a IndexSet<AstId>>,
    /// Active `AstId` → already-reified-`Local` substitutions for the
    /// side-effecting sub-pieces of a compound-assign target, bound once to
    /// `let $ca_N` before the read/write. Reify returns the bound `Local` for
    /// those exact nodes so `arr[bump()] += 1` evaluates `bump()` once while
    /// the place skeleton (index calls, field / deref chain) stays inline and
    /// still writes back. Empty outside `reify_compound_assign`.
    pub(crate) compound_overrides: IndexMap<AstId, TirExpr>,
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
    /// that discovers it. See `reify_local_item`. Keyed by declaration: a
    /// trait default body is reified once per impl inheriting it, yet each
    /// local item in it is declared once.
    pub(crate) pending_local_structs: IndexMap<DefId, TirStruct>,
    /// Local newtype declarations (`Stmt::Item`) — same reasoning as
    /// `pending_local_structs`.
    pub(crate) pending_local_newtypes: IndexMap<DefId, TirNewtype>,
    /// The template shapes reify mints (`reify_template_type`), flushed with
    /// `pending_local_structs` and reify-owned for the same reason.
    pub(crate) pending_template_structs: Vec<TirStruct>,
    /// The coverage plan whose probes reify puts in, under
    /// `wado test --coverage` or `--pgo`. See `probe.rs`.
    pub(crate) coverage: Option<&'a CoverageMap>,
    /// Where the profile marks code cold. See `probe.rs`.
    pub(crate) cold: Option<&'a ColdMap>,
    /// The probe ids reify has put in so far.
    pub(crate) probes_emitted: IndexSet<u32>,
    /// The probe ids of code reify emits no instance of: a tuple `for-of` body
    /// over no elements.
    pub(crate) probes_without_instance: IndexSet<u32>,
    /// The ownership of the body being reified, which says what
    /// [`Self::reify_expr`] marks `Move`. `None` outside a body.
    pub(crate) body_ownership: Option<Rc<BodyOwnership>>,
}

/// What decides the places a body moves.
pub(crate) struct BodyOwnership {
    modes: Modes,
    affine_params: Vec<tir::TypeId>,
}

/// [`ownership::BodyFacts`] as reify reads them, through the active tuple
/// `for-of` overlays: an element's types decide whether a place in the body
/// moves.
struct ReifyFacts<'r, 'a, H: CompilerHost> {
    reify: &'r Reify<'a, H>,
    affine_params: &'r [tir::TypeId],
}

impl<H: CompilerHost> ownership::BodyFacts for ReifyFacts<'_, '_, H> {
    fn consumes_receiver(&self, call: AstId) -> bool {
        self.reify
            .ann_method_dispatch(call)
            .is_some_and(|dispatch| dispatch.consumes_self)
    }

    fn indexes_tuple(&self, base: &ast::Expr) -> bool {
        self.reify
            .ann_expression_types(base.id())
            .is_some_and(|ty| {
                let tt = self.reify.tysys.type_table.borrow();
                matches!(tt.get(tt.peel_refs(ty)), ResolvedType::GenericInstance { def, .. }
                if tt.is_tuple_def(*def))
            })
    }

    fn is_affine(&self, expr: AstId) -> bool {
        self.reify
            .ann_expression_types(expr)
            .is_some_and(|ty| self.reify.tysys.is_affine(ty, self.affine_params))
    }

    fn names_variable(&self, ident: &ast::IdentExpr) -> bool {
        // Read the references of the module `ident` was written in, as
        // `Elaborator::home_module` does: reify replays a trait default body
        // outside its own module.
        let sem = self
            .reify
            .tysys
            .trait_env
            .parse_modules()
            .module_of_space(ident.id.space())
            .map_or(self.reify.sem, |module| {
                &self.reify.all_module_semantics[module]
            });
        sem.bindings.references.get(&ident.id).is_some_and(|def| {
            sem.bindings
                .local_symbols
                .get(def)
                .or_else(|| self.reify.symbols.get(def))
                .is_some_and(ownership::is_variable)
        })
    }
}

/// Call site captured for location literals in defaults.
/// See [`Reify::call_site_location`].
#[derive(Clone)]
pub(crate) struct CallSiteLocation {
    pub(crate) module: ModuleSource,
    pub(crate) span: Span,
    pub(crate) function_name: String,
}

impl<'a, H: CompilerHost> Reify<'a, H> {
    /// The symbol row behind a reference site — see
    /// `Elaborator::symbol_at`, which answers the same way from the same
    /// table, so annotate and reify cannot disagree.
    fn symbol_at(&self, site: AstId) -> Option<&'a Symbol> {
        let def = self.tysys.resolutions.declared_if_walked(site)?;
        self.symbols.get(&self.tysys.resolutions.defs().ast_id(def))
    }

    /// What a call of the function `site` names instantiates.
    fn template_at(&self, site: AstId) -> Option<TemplateId> {
        let def = self.tysys.resolutions.declared_if_walked(site)?;
        Some(self.tysys.signatures.declared_template(def))
    }

    /// Construct a per-module `Reify` for the orchestration driver. The `tysys`
    /// clone is the shallow one [`TypeSystem`] supports. `current_module_source`
    /// / `current_module_items` are placeholders here — [`Self::reify_module`]
    /// overwrites them; keeping them on the struct saves threading them through
    /// every method signature.
    pub(crate) fn new(
        tysys: TypeSystem,
        sem: &'a ModuleSemantics,
        all_module_semantics: &'a IndexMap<ModuleSource, Rc<ModuleSemantics>>,
        symbols: &'a SymbolTable,
        loaded_modules: &'a IndexMap<ModuleSource, Module>,
        logger: &'a Logger<'a, H>,
        emit_live: Option<&'a IndexSet<AstId>>,
    ) -> Self {
        Self {
            tysys,
            sem,
            types: &sem.types,
            all_module_semantics,
            symbols,
            loaded_modules,
            logger,
            current_module_source: ModuleSource::entry_point_uninitialized(),
            emitted_module_source: ModuleSource::entry_point_uninitialized(),
            current_module_items: &[],

            tuple_overlay_stack: Vec::new(),
            tuple_overlay_visits: IndexMap::default(),
            emit_live,
            compound_overrides: IndexMap::default(),
            call_site_location: None,
            pending_local_structs: IndexMap::default(),
            pending_template_structs: Vec::new(),
            pending_local_newtypes: IndexMap::default(),
            coverage: None,
            cold: None,
            probes_emitted: IndexSet::default(),
            probes_without_instance: IndexSet::default(),
            body_ownership: None,
        }
    }

    /// Run `reify` over `body`, a function's or a test's, with the places it
    /// moves known. `affine_params` are its `affine` type parameters.
    fn with_body_ownership<R>(
        &mut self,
        body: &ast::Block,
        affine_params: Vec<tir::TypeId>,
        reify: impl FnOnce(&mut Self) -> R,
    ) -> R {
        self.with_ownership(
            affine_params,
            |facts| ownership::body_modes(body, facts),
            reify,
        )
    }

    /// [`Self::with_body_ownership`] for an expression reified where no body
    /// is: a default, a global's initializer, an associated constant.
    fn with_expr_ownership<R>(
        &mut self,
        expr: &ast::Expr,
        reify: impl FnOnce(&mut Self) -> R,
    ) -> R {
        self.with_ownership(
            Vec::new(),
            |facts| ownership::expr_modes(expr, facts),
            reify,
        )
    }

    fn with_ownership<R>(
        &mut self,
        affine_params: Vec<tir::TypeId>,
        walk: impl FnOnce(&ReifyFacts<'_, 'a, H>) -> Modes,
        reify: impl FnOnce(&mut Self) -> R,
    ) -> R {
        let modes = walk(&ReifyFacts {
            reify: self,
            affine_params: &affine_params,
        });
        let ownership = Some(Rc::new(BodyOwnership {
            modes,
            affine_params,
        }));
        util::replaced(self, |this| &mut this.body_ownership, ownership, reify).0
    }

    /// Answer `ask` about the body being reified; outside one, nothing moves.
    fn ask_ownership(&self, ask: impl FnOnce(&Modes, &ReifyFacts<'_, 'a, H>) -> bool) -> bool {
        self.body_ownership.as_ref().is_some_and(|body| {
            ask(
                &body.modes,
                &ReifyFacts {
                    reify: self,
                    affine_params: &body.affine_params,
                },
            )
        })
    }

    /// Whether `expr` is a place the body being reified moves.
    fn moves(&self, expr: &ast::Expr) -> bool {
        self.ask_ownership(|modes, facts| modes.moves(expr, facts))
    }

    /// Whether the body being reified takes ownership of the value of `expr`,
    /// so a local reify binds it to moves where `expr` stood.
    fn takes(&self, expr: &ast::Expr) -> bool {
        self.ask_ownership(|modes, facts| modes.takes(expr, facts))
    }

    fn is_affine(&self, expr: &ast::Expr) -> bool {
        self.ask_ownership(|_, facts| facts.is_affine(expr.id()))
    }

    /// `read` marked moved where its value is affine. `read` reads a local
    /// reify bound ahead of its use, in a position that takes the value.
    fn handed_on(&self, read: TirExpr) -> TirExpr {
        if self.is_affine_type(read.type_id) {
            read.moved()
        } else {
            read
        }
    }

    /// Whether a value of `ty` is move-only in the body being reified.
    fn is_affine_type(&self, ty: TypeId) -> bool {
        self.ask_ownership(|_, facts| self.tysys.is_affine(ty, facts.affine_params))
    }

    /// The body fact `map` records for `id` in the walk reify is replaying:
    /// the active tuple `for-of` overlays innermost-first, then the module's
    /// own walk. Every read of a body fact goes through here, because annotate
    /// peeled each element's facts out of the module's own maps.
    fn ann<V: Clone>(&self, map: fn(&BodyFacts) -> &IndexMap<AstId, V>, id: AstId) -> Option<V> {
        self.ann_ref(map, id).cloned()
    }

    /// [`Self::ann`] without the clone, for a fact reify only borrows. The
    /// borrow outlives the call, so a walk it names can be pushed onto
    /// [`Self::tuple_overlay_stack`].
    fn ann_ref<V>(&self, map: fn(&BodyFacts) -> &IndexMap<AstId, V>, id: AstId) -> Option<&'a V> {
        self.tuple_overlay_stack
            .iter()
            .rev()
            .copied()
            .chain(std::iter::once(&self.types.body))
            .find_map(|facts| map(facts).get(&id))
    }

    /// Run `body` with `overlay`'s facts answering ahead of the enclosing ones.
    fn with_overlay<R>(
        &mut self,
        overlay: Option<&'a BodyFacts>,
        body: impl FnOnce(&mut Self) -> R,
    ) -> R {
        let mut stack = self.tuple_overlay_stack.clone();
        stack.extend(overlay);
        util::replaced(self, |reify| &mut reify.tuple_overlay_stack, stack, body).0
    }

    with_body_facts!(reify_annotation_accessors);

    /// Recorded type of an expression, reporting an indefinite one as absent
    /// so the node falls back to its `expected_type`.
    fn ann_expression_types(&self, id: AstId) -> Option<tir::TypeId> {
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
            ann_method_impl_type_params => method_impl_type_params: Vec<TirTypeParam>,
            ann_fn_param_types => fn_param_types: Vec<tir::TypeId>,
            ann_fn_affine_params => fn_affine_params: Vec<tir::TypeId>,
            ann_fn_return_type => fn_return_types: tir::TypeId,
            ann_effect_ops => effect_ops: Vec<TirEffectOp>,
            ann_decl_type_params => decl_type_params: Vec<TirTypeParam>,
            ann_struct_field_types => struct_field_types: Vec<tir::TypeId>,
            ann_method_names => method_names: MethodNames,
        }
    }

    /// A [`TypeLookup`] from the current module, without annotate's use→def recording.
    fn type_lookup(&self) -> TypeLookup<'_> {
        // A function-local item is reached by declaration through the local data tables:
        // annotate clears `fn_local_items`, and reify never refills it.
        self.tysys.type_lookup(
            &self.current_module_source,
            &self.sem.imports.namespace_imports,
            &self.sem.decls,
        )
    }

    /// A function type's effects, read off the sites the resolve walk answered.
    /// Annotate already reported a name that reaches no effect.
    fn reify_effects(&self, ft: &ast::FunctionType) -> Vec<EffectRef> {
        ft.effects
            .iter()
            .map(|effect| {
                self.tysys
                    .resolutions
                    .effect_named(effect, &self.current_module_source)
            })
            .collect()
    }

    /// A global's declared type, resolved without recording a use→def edge;
    /// annotate recorded them during its body walk.
    // Read in the declaration's own module scope, never in the type parameters
    // of whatever body referenced the global.
    fn resolve_global_type(&mut self, ty: &ast::Type) -> TypeId {
        let lookup = self.type_lookup();
        let resolved = Elaborator::<H>::resolve_type_static(
            ty,
            &mut self.tysys.type_table.borrow_mut(),
            &lookup,
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
                let effects = self.reify_effects(ft);
                let rebuilt = match self.tysys.type_table.borrow().get(resolved) {
                    ResolvedType::Function {
                        is_mut,
                        params,
                        return_type,
                        ..
                    } => ResolvedType::Function {
                        is_mut: *is_mut,
                        params: params.clone(),
                        return_type: *return_type,
                        effects,
                    },
                    _ => return resolved,
                };
                self.tysys.type_table.borrow_mut().intern(rebuilt)
            }
            _ => resolved,
        }
    }

    /// True when liveness gating is active and nothing the emitted program
    /// keeps reaches `id`, so it never reaches monomorphization.
    fn is_dead_item(&self, id: AstId) -> bool {
        self.emit_live.is_some_and(|live| !live.contains(&id))
    }

    pub(crate) fn reify_module(
        &mut self,
        module: &'a Module,
        module_source: ModuleSource,
    ) -> Result<TirModule, Bail> {
        self.current_module_source = module_source.clone();
        self.emitted_module_source = module_source.clone();
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
                    let mut tir_func = self.reify_function(func);
                    let twin = unchecked_twin(
                        func,
                        &mut tir_func,
                        &self.current_module_source,
                        &mut self.tysys.type_table.borrow_mut(),
                    );
                    tir_module.add_function(tir_func);
                    if let Some(twin) = twin {
                        tir_module.add_function(twin);
                    }
                }
                Item::Struct(struct_decl) => {
                    tir_module.add_struct(self.reify_struct(struct_decl));
                }
                Item::Impl(impl_block) => {
                    if let Some(tir_impl) = self.reify_impl_decl(impl_block) {
                        tir_module.add_impl(tir_impl);
                    }
                    // Reify is the sole producer of
                    // trait default-method `TirFunction`s, synthesised here
                    // from the per-impl facts the body walk recorded on
                    // `sem.default_method_facts`.
                    let mut methods = self.reify_impl(impl_block);
                    methods.extend(self.reify_impl_default_methods(impl_block));
                    let bound = bind_trait_contracts(
                        methods,
                        &self.contract_bindings(impl_block),
                        &self.current_module_source,
                        &mut self.tysys.type_table.borrow_mut(),
                    );
                    for tir_func in bound {
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
                    if let Some((tir_global, init_fn)) = self.reify_global(global_decl) {
                        tir_module.globals.push(tir_global);
                        if let Some(init_fn) = init_fn {
                            tir_module.add_function(init_fn);
                        }
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
                        tir_module.add_function(self.reify_function(&method));
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
        for local_struct in std::mem::take(&mut self.pending_local_structs).into_values() {
            tir_module.add_struct(local_struct);
        }
        for local_newtype in std::mem::take(&mut self.pending_local_newtypes).into_values() {
            tir_module.add_newtype(local_newtype);
        }
        for template_struct in std::mem::take(&mut self.pending_template_structs) {
            tir_module.add_struct(template_struct);
        }

        tir_module.wasm_module = module.wasm_module().map(String::from);

        self.assert_probes_complete();
        self.logger.ok_or_bail(tir_module)
    }

    // ─────────────────────────────────────────────────────────────────
    // Decl-only items: read from `tysys.data` and produce TIR without
    // consulting `TypeAnnotations`.
    // ─────────────────────────────────────────────────────────────────

    /// Reify an `enum E { … }` declaration. Pure projection from the
    /// AST shape; cases keep their declared index.
    fn reify_enum(&self, enum_decl: &ast::EnumDecl) -> TirEnum {
        TirEnum {
            def: self.tysys.def_at(enum_decl.id),
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

    /// Reify a `flags F { … }` declaration, typed as
    /// [`DataDecls::declare_flags`](super::types::DataDecls::declare_flags) interned it.
    fn reify_flags(&self, flags_decl: &ast::FlagsDecl) -> Option<TirFlags> {
        let def = self.tysys.def_at(flags_decl.id);
        let info = self.tysys.data.flags_cases.get(&def)?;
        Some(TirFlags {
            def,
            name: flags_decl.name.clone(),
            module_source: self.current_module_source.clone(),
            visibility: flags_decl.visibility,
            type_id: info.type_id,
            members: flags_decl
                .flags
                .iter()
                .zip(&info.members)
                .map(|(m, member)| TirFlagsMember {
                    name: member.name.clone(),
                    bitmask: member.bitmask,
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
    fn declared_type_params(params: &[ast::GenericParam]) -> Vec<TirTypeParam> {
        params
            .iter()
            .enumerate()
            .map(|(index, p)| TirTypeParam {
                name: p.name.clone(),
                is_effect: p.is_effect,
                is_pack: p.is_pack,
                bounds: p.real_bounds().iter().map(|b| b.name.clone()).collect(),
                default: None,
                index: index as u32,
                projected: false,
            })
            .collect()
    }

    /// Reify a `type N = T;` declaration. A generic one names no single type —
    /// each instantiation is its own, minted on demand by
    /// `make_newtype_instance` — so it lands here without one, carrying the
    /// parameters its synthesized impls are written over.
    fn reify_newtype(&self, newtype_decl: &ast::Newtype) -> Option<TirNewtype> {
        let def = self.tysys.def_at(newtype_decl.id);
        let generic = !newtype_decl.type_params.is_empty();
        let type_id = self.tysys.data.newtypes.get(&def).copied();
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
            span: newtype_decl.span,
        })
    }

    // ─────────────────────────────────────────────────────────────────
    // Items with sub-declaration walks but no function bodies.
    // ─────────────────────────────────────────────────────────────────

    /// Reify a `struct S { … }` declaration.
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
        let fields = self.reify_fields(struct_decl, &field_types);

        // Single source of truth: the body walk projected these type
        // params with each default resolved while the decl's type-param scope
        // was alive; read them back rather than re-resolving the defaults.
        let type_params = self
            .ann_decl_type_params(struct_decl.id)
            .expect("resolve_struct records the type params for every struct reify emits");

        let wire_name_policy = wire_name_policy_of(&struct_decl.attrs);

        TirStruct {
            def: StructDef::Decl(self.tysys.def_at(struct_decl.id)),
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

    /// `struct_decl`'s fields, typed by `field_types` in declaration order.
    fn reify_fields(
        &mut self,
        struct_decl: &ast::StructDecl,
        field_types: &[TypeId],
    ) -> Vec<TirField> {
        assert_eq!(field_types.len(), struct_decl.fields.len());
        // A default resolves with no `self` and no other field in scope, in a
        // context named as `resolve_struct` names it, so both see the same TIR.
        let mut field_ctx =
            FunctionContext::new(TypeTable::UNIT, format!("struct:{}", struct_decl.name));
        struct_decl
            .fields
            .iter()
            .zip(field_types)
            .enumerate()
            .map(|(index, (field, &type_id))| {
                self.reify_struct_field(field, index, type_id, &mut field_ctx)
            })
            .collect()
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

    /// Reify a `struct` declared in a function body. Field attributes and defaults
    /// come from the AST, as for a top-level struct: `StructFieldInfo` carries neither.
    fn reify_local_struct(&mut self, struct_decl: &ast::StructDecl) {
        let def = self.tysys.def_at(struct_decl.id);
        if self.pending_local_structs.contains_key(&def) {
            return;
        }
        let info = &self.sem.decls.local.struct_fields[&def];
        let name = info.name.clone();
        let field_types: Vec<TypeId> = info.fields.iter().map(|&(_, ty, _)| ty).collect();
        let fields = self.reify_fields(struct_decl, &field_types);
        // Single source of truth, as for a top-level struct: the body walk
        // projected these with the struct's own type-param scope alive.
        let type_params = self.ann_decl_type_params(struct_decl.id).expect(
            "resolve_local_struct records the type params for every local struct reify emits",
        );
        self.pending_local_structs.insert(
            def,
            TirStruct {
                def: StructDef::Decl(def),
                type_args: Vec::new(),
                name,
                module_source: self.current_module_source.clone(),
                visibility: ast::Visibility::Private,
                type_params,
                monomorph_info: None,
                fields,
                span: struct_decl.span,
                wire_name_policy: wire_name_policy_of(&struct_decl.attrs),
            },
        );
    }

    /// Reify a newtype declared in a function body.
    fn reify_local_newtype(&mut self, newtype_decl: &ast::Newtype) {
        let def = self.tysys.def_at(newtype_decl.id);
        let generic = !newtype_decl.type_params.is_empty();
        let type_id = self.sem.decls.local.newtypes.get(&def).copied();
        if (!generic && type_id.is_none()) || self.pending_local_newtypes.contains_key(&def) {
            return;
        }
        self.pending_local_newtypes.insert(
            def,
            TirNewtype {
                name: mangle_local_item_name(&newtype_decl.name, newtype_decl.id),
                module_source: self.current_module_source.clone(),
                visibility: ast::Visibility::Private,
                def,
                type_params: Self::declared_type_params(&newtype_decl.type_params),
                type_id: type_id.filter(|_| !generic),
                span: newtype_decl.span,
            },
        );
    }

    /// Reify a `variant V<T> { … }` declaration.
    fn reify_variant_decl(&mut self, variant_decl: &ast::VariantDecl) -> TirVariantDecl {
        let def = self.tysys.def_at(variant_decl.id);
        let case_info = &self.tysys.data.variant_cases[&def];

        let cases: Vec<tir::TirVariantCase> = variant_decl
            .cases
            .iter()
            .zip(&case_info.cases)
            .enumerate()
            .map(|(index, (case, data))| tir::TirVariantCase {
                name: case.name.clone(),
                index: index as u32,
                payload: data.payload,
                span: case.span,
                wire_name_override: wire_name_override_of(&case.attrs),
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

        let wire_tag = wire_value_of(&variant_decl.attrs, "tag");
        if let Some(tag) = &wire_tag {
            self.check_internal_tag(variant_decl, tag, &cases);
        }

        TirVariantDecl {
            def,
            name: variant_decl.name.clone(),
            module_source: self.current_module_source.clone(),
            visibility: variant_decl.visibility,
            type_params,
            cases,
            span: variant_decl.span,
            wire_name_policy: wire_name_policy_of(&variant_decl.attrs),
            wire_tag,
        }
    }

    /// An internally tagged case is written as an object holding `tag` beside
    /// its payload's fields, so the payload is a struct, or nothing, and writes
    /// no field under `tag`.
    fn check_internal_tag(
        &self,
        variant_decl: &ast::VariantDecl,
        tag: &str,
        cases: &[tir::TirVariantCase],
    ) {
        let unit = self.tysys.type_table.borrow().type_key(TypeTable::UNIT);
        for (case, reified) in variant_decl.cases.iter().zip(cases) {
            if self.tysys.type_table.borrow().type_key(reified.payload) == unit {
                continue;
            }
            let case_path = format!("{}::{}", variant_decl.name, case.name);
            let Some(payload) = self.derived_struct_decl_of(reified.payload) else {
                let payload = self.tysys.type_table.borrow().type_name(reified.payload);
                self.error_at(
                    Code::AttrMisuse,
                    case.span,
                    format!(
                        "`{case_path}` carries `{payload}`, but a case of a variant with \
                         `#[wire(tag)]` carries nothing or a struct its derivation writes"
                    ),
                );
                continue;
            };
            let policy = wire_name_policy_of(&payload.attrs);
            for field in &payload.fields {
                let wire_name = NamePolicy::wire_name(
                    wire_name_override_of(&field.attrs).as_deref(),
                    policy,
                    &field.name,
                );
                if wire_name == tag {
                    self.error_at(
                        Code::AttrMisuse,
                        case.span,
                        format!(
                            "`{case_path}` writes its payload's field `{}` as `{tag}`, \
                             the key its tag is written under",
                            field.name
                        ),
                    );
                }
            }
        }
    }

    /// The struct declaration `ty` instantiates, through any newtype over it,
    /// where the derivation reads and writes it as a struct: no link of `ty`
    /// has a hand-written `Serialize` or `Deserialize`, whose shape nothing
    /// here can see. `String` and `List` are declared as structs and are such.
    fn derived_struct_decl_of(&self, ty: TypeId) -> Option<&'a ast::StructDecl> {
        let written = [CompilerItem::Serialize, CompilerItem::Deserialize]
            .into_iter()
            .filter_map(|item| self.tysys.compiler_trait_def(item))
            .any(|trait_| self.tysys.impl_link(ty, trait_).is_some());
        if written {
            return None;
        }
        self.struct_decl_of(ty)
    }

    /// The struct declaration `ty` instantiates, through any newtype over it,
    /// or `None` where `ty` is not a declared struct.
    fn struct_decl_of(&self, ty: TypeId) -> Option<&'a ast::StructDecl> {
        let def = {
            let type_table = self.tysys.type_table.borrow();
            let head = type_table.newtype_representation(ty).unwrap_or(ty);
            type_table.nominal_def(head)?
        };
        let defs = self.tysys.resolutions.defs();
        let target = defs.ast_id(def);
        self.loaded_modules
            .get(defs.module(def))?
            .items
            .iter()
            .find_map(|item| match item {
                ast::Item::Struct(decl) if decl.id == target => Some(decl),
                _ => None,
            })
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
            .expect("`record_effect_ops` records op signatures for every effect reify emits");
        tir::TirEffect {
            head: DeclaredHead::new(self.tysys.resolutions.defs(), self.tysys.def_at(decl.id)),
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
            .expect("`record_effect_ops` records op signatures for every resource reify emits");
        tir::TirResource {
            head: DeclaredHead::new(self.tysys.resolutions.defs(), self.tysys.def_at(decl.id)),
            visibility: decl.visibility,
            operations,
            is_generic: !decl.type_params.is_empty(),
            span: decl.span,
        }
    }

    // ─────────────────────────────────────────────────────────────────
    // Items with function bodies — the bulk of reify_*.
    // ─────────────────────────────────────────────────────────────────

    /// The result an `async fn` delivers through `task return`. Annotate
    /// records it for free functions only; a method falls back to `declared`.
    fn declared_task_return(&self, func: &ast::Function, declared: TypeId) -> Option<TypeId> {
        func.is_async.then(|| {
            self.types
                .function_task_returns
                .get(&func.id)
                .copied()
                .unwrap_or(declared)
        })
    }

    /// Reify a free function.
    fn reify_function(&mut self, func: &ast::Function) -> TirFunction {
        self.reify_callable(func, func.name.clone())
    }

    /// A function or method as its declaring walk resolved it, not yet placed on
    /// an impl. Parameters are added in declaration order, pinning the walk order.
    fn reify_callable(&mut self, func: &ast::Function, display_name: String) -> TirFunction {
        let return_type = self
            .ann_fn_return_type(func.id)
            .expect("the declaring walk records the return type for every function reify emits");

        let mut ctx = FunctionContext::new(return_type, display_name.clone());
        let task_return_type = self.declared_task_return(func, return_type);
        ctx.is_async = func.is_async;
        ctx.task_return_type = task_return_type;

        let param_types = self
            .ann_fn_param_types(func.id)
            .expect("the declaring walk records param types for every function reify emits");
        let mut params = Vec::with_capacity(func.params.len());
        for (p_idx, param) in func.params.iter().enumerate() {
            let type_id = *param_types
                .get(p_idx)
                .expect("the declaring walk records one param type per func.params entry");
            let name = if matches!(param.self_kind, ast::SelfKind::None) {
                param.name.clone()
            } else {
                "self".to_string()
            };
            // A default is reified at each call site: one reified here would put
            // its value local in this `ctx`, shadowing the parameter at -O0.
            let local_index = ctx.add_local_at(
                name.clone(),
                type_id,
                param.is_mut,
                Some(param.id),
                param.name_span,
            );
            params.push(TirParam {
                name,
                type_id,
                local_index,
                is_mut: param.is_mut,
                is_mut_ref: false,
                span: param.span,
            });
        }

        let body = func.body.as_ref().map(|b| {
            let affine_params = self
                .ann_fn_affine_params(func.id)
                .expect("a walked body records its affine parameters");
            self.with_body_ownership(b, affine_params, |this| this.reify_block(b, &mut ctx, None))
        });

        // Projected while the type-param scope was alive, so defaults are
        // resolved; after it is torn down they cannot be.
        let type_params = self
            .ann_decl_type_params(func.id)
            .expect("the declaring walk records the type params for every function reify emits");
        let declared = self.reify_declared_facts(func, &params, return_type);
        let immediates = self.reify_immediate_attrs(func, &params);
        if body.is_some() {
            self.reject_bodyless_attrs_on_body(&func.attrs);
        }

        TirFunction {
            module_source: ModuleSource::default(),
            name: display_name,
            def_id: self.tysys.resolutions.defs().of_ast_id(func.id),
            visibility: func.visibility,
            is_export: func.is_export,
            is_async: func.is_async,
            type_params,
            impl_type_params: vec![],
            impl_origin: None,
            monomorph_info: None,
            method_info: None,
            params,
            return_type,
            task_return_type,
            effects: self.types.function_effects.get(&func.id).cloned().expect(
                "the declaring walk records function_effects for every function reify emits",
            ),
            immediates,
            declared,
            body,
            span: func.span,
            local_count: ctx.local_count(),
            locals: ctx.locals.clone(),
            address_taken_locals: ctx.address_taken_locals,
            is_cm_binding: false,
            is_dispatch_wrapper: false,
            is_cm_export: false,
            is_ambient: extract_is_ambient_attr(&func.attrs),
            inline_hint: extract_inline_hint_attr(&func.attrs),
            compiler_item: extract_compiler_item(
                &func.attrs,
                func.span,
                &self.current_module_source,
                self.logger,
            ),
            export_name: extract_export_name_attr(&func.attrs),
            allocator_tag: extract_allocator_tag_attr(&func.attrs),
            kind: tir::FunctionKind::Regular,
            return_abi: tir::ReturnAbi::Single,
        }
    }

    /// Reify the impl block's own declaration — its identity and rest
    /// clause, with no methods (those go through [`Self::reify_impl`] into
    /// `TirModule::functions`). A block whose only content is a rest clause
    /// still produces a record, which is the point: the effect-dispatch
    /// synthesis has no method to learn about it from.
    fn reify_impl_decl(&mut self, impl_block: &ast::ImplBlock) -> Option<TirImpl> {
        if impl_block.is_synthesize_request {
            return None;
        }
        let facts = self.types.impl_facts.get(&impl_block.id)?;

        // Derive the target's name the way `reify_method` derives it for this
        // block's methods, so the block and its methods agree on the key the
        // effect-dispatch handler index is built from.
        let mut naming = LocalMethodName::of(
            facts.receiver.clone(),
            facts.trait_name.clone(),
            String::new(),
        );
        if let Some(owner) = facts.concrete_owner.as_ref() {
            naming = naming.at_owner(owner);
        }

        Some(TirImpl {
            trait_canonical: facts
                .trait_name
                .as_ref()
                .and_then(|fq| fq.declared_head().cloned()),
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
        let Some(facts) = self.types.impl_facts.get(&impl_key).cloned() else {
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
        let origin = self.impl_origin(impl_block);

        let live: Vec<&ast::Function> = impl_block
            .methods
            .iter()
            .filter(|method| !self.is_dead_item(method.id))
            .collect();
        let mut out = Vec::with_capacity(live.len());
        for method in live {
            let mut tir_func = self.reify_method(method, &facts, concrete_owner.as_ref(), origin);
            let twin = unchecked_twin(
                method,
                &mut tir_func,
                &self.current_module_source,
                &mut self.tysys.type_table.borrow_mut(),
            );
            out.push(tir_func);
            out.extend(twin);
        }
        out
    }

    fn impl_origin(&self, impl_block: &ast::ImplBlock) -> DefId {
        self.tysys.resolutions.defs().def_at(impl_block.id)
    }

    /// Synthesise a `Struct^Trait::method` `TirFunction` for each default method
    /// the impl does not override, reading the body-walk facts from
    /// `sem.default_method_facts[(impl_block.id, default_method.id)]`. The
    /// module perspective is swapped to the trait module for the walk, since the
    /// facts are keyed by `AstId`s that name it.
    fn reify_impl_default_methods(&mut self, impl_block: &ast::ImplBlock) -> Vec<TirFunction> {
        if impl_block.is_synthesize_request {
            return Vec::new();
        }
        let Some(trait_ast) = impl_block.trait_type.as_ref() else {
            return Vec::new();
        };
        let impl_key = impl_block.id;
        let Some(facts) = self.types.impl_facts.get(&impl_key).cloned() else {
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
        let Some(trait_decl) = self.tysys.resolutions.head_decl(trait_ast) else {
            return Vec::new();
        };
        let Some(trait_sig) =
            trait_sig_of_with(trait_decl, &self.tysys.trait_env, &self.tysys.signatures)
        else {
            return Vec::new();
        };
        let provided: hashmap::IndexSet<&str> =
            impl_block.methods.iter().map(|m| m.name.as_str()).collect();
        let default_methods: Vec<std::rc::Rc<ast::Function>> = trait_sig
            .default_methods()
            .filter(|(name, _)| !provided.contains(name))
            .map(|(_, body)| std::rc::Rc::clone(body))
            .collect();
        let trait_module = trait_sig.module.clone();
        let origin = self.impl_origin(impl_block);

        let trait_items: &'a [ast::Item] = self
            .loaded_modules
            .get(&trait_module)
            .map(|m| m.items.as_slice())
            .unwrap_or(&[]);

        let mut out = Vec::with_capacity(default_methods.len());
        for default_method in &default_methods {
            let key = (impl_block.id, default_method.id);
            let sem = self.sem;
            let Some(walk) = sem.default_method_facts.get(&key) else {
                // Combined walk did not record a synthesis for this default
                // method (e.g. `resolve_method` returned `None` in
                // error-recovery). Skip — reify produces no TIR for it.
                continue;
            };

            let mut tir_func = self.with_perspective(
                trait_module.clone(),
                trait_items,
                sem,
                &walk.types,
                |this| this.reify_method(default_method, &facts, concrete_owner.as_ref(), origin),
            );

            // No declaring walk recorded `method_names` for a synthesized default.
            tir_func.name = MethodName::format_local(
                concrete_owner.as_ref().unwrap_or(&struct_name),
                Some(&trait_name_mangled),
                &default_method.name,
            );
            // A trait's default method is part of the implementation, so it is
            // public whatever its declaration says.
            tir_func.visibility = Visibility::Public;
            out.push(tir_func);
        }

        out
    }

    /// Reify a single method inside an `impl` block: a [`Self::reify_callable`]
    /// placed on the impl its recorded [`ImplFacts`] describe.
    fn reify_method(
        &mut self,
        func: &ast::Function,
        facts: &ImplFacts,
        // `Some("List<u8>")` when the impl is on a concrete generic
        // instantiation. The method is then a per-instantiation *concrete*
        // function: named `List<u8>::method`, with no impl type params and no
        // monomorphization, so distinct instantiations stay distinct and call
        // sites resolve it directly (mirroring a monomorphized instance).
        concrete_owner: Option<&FqTypeName>,
        origin: DefId,
    ) -> TirFunction {
        let mut impl_type_params: Vec<TirTypeParam> =
            self.ann_method_impl_type_params(func.id).expect(
                "resolve_method records the impl-type-param scheme for every \
                 impl method reify emits",
            );

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
            mangled_name = MethodName::format_local(owner, facts.trait_name.as_ref(), &func.name);
            method_info = method_info.at_owner(owner);
            impl_type_params = Vec::new();
        }

        let mut tir_func = self.reify_callable(func, display_name);
        tir_func.name = mangled_name;
        tir_func.is_export = false;
        tir_func.impl_origin = Some(origin);
        tir_func.impl_type_params = impl_type_params;
        tir_func.method_info = Some(method_info);
        tir_func
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
        let meta = test_decl.metadata(module_is_todo);
        let ast::TestMetadata {
            expect_trap,
            is_todo,
            timeout_ms,
            is_synopsis,
            is_workload,
        } = meta;
        let function_name = test_function_name(&meta, test_index, test_decl.name.as_deref());

        let return_type = TypeTable::UNIT;
        let mut ctx = FunctionContext::new(return_type, function_name.clone());
        let body = self.with_body_ownership(&test_decl.body, Vec::new(), |this| {
            this.reify_block(&test_decl.body, &mut ctx, None)
        });

        let tir_func = TirFunction {
            module_source: ModuleSource::default(),
            name: function_name.clone(),
            def_id: None,
            visibility: Visibility::Private,
            is_export: false,
            is_async: false,
            type_params: vec![],
            impl_type_params: vec![],
            impl_origin: None,
            monomorph_info: None,
            method_info: None,
            params: vec![],
            return_type,
            task_return_type: None,
            effects: vec![],
            immediates: vec![],
            declared: None,
            body: Some(body),
            span: test_decl.span,
            local_count: ctx.local_count(),
            locals: ctx.locals.clone(),
            address_taken_locals: ctx.address_taken_locals,
            is_cm_binding: false,
            is_dispatch_wrapper: false,
            is_cm_export: false,
            is_ambient: false,
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
            is_workload,
        };

        Some((tir_func, tir_test))
    }

    /// Reify a `global g: T = expr;` declaration. The declared type
    /// was already resolved by `annotate_decls` and lives on
    /// `sem.decls.current_module_globals`; reify reads it back and
    /// walks the initializer through a minimal `FunctionContext`.
    /// `is_nullable` is populated by the lower phase (kept `false` here,
    /// matching `Elaborator::resolve_global`).
    fn reify_global(
        &mut self,
        global_decl: &ast::GlobalDecl,
    ) -> Option<(TirGlobal, Option<TirFunction>)> {
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
        let initializer = self.with_expr_ownership(&global_decl.initializer, |this| {
            this.reify_expr(&global_decl.initializer, &mut ctx, Some(ty))
        });
        let param = self.reify_param_attr(global_decl);
        let span = global_decl.span;

        let frame = LocalFrame {
            locals: std::mem::take(&mut ctx.locals),
            address_taken: std::mem::take(&mut ctx.address_taken_locals),
        };
        let (init, init_fn) =
            self.split_global_initializer(&global_decl.name, ty, initializer, frame, span);
        Some((
            TirGlobal {
                name: global_decl.name.clone(),
                ty,
                init,
                param,
                wado_mutable: global_decl.mutable,
                visibility: global_decl.visibility,
                module_source: self.current_module_source.clone(),
                span,
            },
            init_fn,
        ))
    }

    /// Split an initializer into what the Wasm slot can hold and what needs a
    /// body. A body lives only in `module.functions`, so no later pass can walk
    /// the module's functions and miss an initializer.
    fn split_global_initializer(
        &self,
        name: &str,
        ty: TypeId,
        initializer: TirExpr,
        frame: LocalFrame,
        span: Span,
    ) -> (GlobalInit<TirExpr>, Option<TirFunction>) {
        let type_table = self.tysys.type_table.borrow();
        if tir::is_constant_initializer(&initializer, &type_table) {
            assert!(
                frame.locals.is_empty(),
                "a constant initializer allocates no local"
            );
            return (GlobalInit::Direct(initializer), None);
        }
        let placeholder = tir::placeholder_value(ty, &type_table, span);
        let init_fn = TirFunction::synthesized(
            self.current_module_source.clone(),
            global_init_function(name),
            ty,
            tir::initializer_body(initializer, span),
            frame,
            span,
        );
        (GlobalInit::Deferred(placeholder), Some(init_fn))
    }

    /// Each `contract` clause holder `impl_block`'s trait declares, paired
    /// with the method of the block that answers the one it holds the clauses
    /// of: the block's own, else the trait's default.
    fn contract_bindings(&self, impl_block: &ast::ImplBlock) -> Vec<(DefId, DefId)> {
        let Some(decl) = impl_block
            .trait_type
            .as_ref()
            .and_then(|ty| self.tysys.resolutions.head_decl(ty))
            .and_then(|decl| self.trait_decl(decl))
        else {
            return Vec::new();
        };
        let defs = self.tysys.resolutions.defs();
        decl.methods
            .iter()
            .filter_map(|holder| {
                let held_id = holder.holds_contracts_of?;
                let held = decl
                    .methods
                    .iter()
                    .find(|m| m.id == held_id)
                    .expect("a holder follows the method it holds the clauses of");
                let answering = impl_block
                    .methods
                    .iter()
                    .find(|m| m.name == held.name)
                    .map_or(held_id, |m| m.id);
                Some((defs.def_at(holder.id), defs.def_at(answering)))
            })
            .collect()
    }

    /// The trait `def` declares.
    fn trait_decl(&self, def: DefId) -> Option<&'a ast::TraitDecl> {
        Contracts {
            resolutions: &self.tysys.resolutions,
            modules: self.loaded_modules,
        }
        .trait_decl(def)
    }

    /// Report an error at `span` in the module reify stands in.
    fn error_at(&self, code: Code, span: Span, message: String) {
        let _ = self.logger.error_in(
            &self.current_module_source,
            Diagnostic {
                severity: Severity::Error,
                code,
                message,
                span: Some(DiagnosticSpan::from_span(&span, None)),
            },
        );
    }

    /// Report a malformed attribute at the attribute's own span.
    fn attr_error(&self, code: Code, attr: &ast::Attribute, message: String) {
        self.error_at(code, attr.span, message);
    }

    /// Report each attribute that describes a declaration with no body, written
    /// on a function that has one. Its arguments were read already, so a
    /// malformed one reports that too.
    fn reject_bodyless_attrs_on_body(&self, attrs: &[ast::Attribute]) {
        for (attr, name, bodyless) in attribute::bodyless(attrs) {
            self.attr_error(
                Code::AttrMisuse,
                attr,
                format!(
                    "#[{name}] belongs to a declaration with no body: {}",
                    bodyless.on_body
                ),
            );
        }
    }

    /// A declared struct field as TIR: its attributes read, its default reified.
    fn reify_struct_field(
        &mut self,
        field: &ast::StructField,
        index: usize,
        type_id: TypeId,
        field_ctx: &mut FunctionContext,
    ) -> TirField {
        let default_expr = field
            .default
            .as_ref()
            .map(|default_ast| Box::new(self.reify_expr(default_ast, field_ctx, Some(type_id))));
        TirField {
            name: field.name.clone(),
            visibility: field.visibility,
            type_id,
            index: index as u32,
            span: field.span,
            is_secret: field.attrs.iter().any(|a| a.name == SECRET),
            wire_name_override: wire_name_override_of(&field.attrs),
            serde_default: field.default.is_some(),
            serde_positional: field
                .attrs
                .iter()
                .any(|a| a.name == WIRE && a.has_arg("positional")),
            default_expr,
        }
    }

    /// Extract and structurally validate a `#[param]` attribute on a global.
    ///
    /// Returns `Some(ParamSpec)` for a well-formed `#[param]`, `None` when the
    /// global has no `#[param]` or the attribute is malformed (in which case a
    /// structural error is emitted; compilation bails at `ok_or_bail`). The
    /// resolution itself (overrides, env, conversion) happens later in the
    /// param-resolution pass — see `wep-2026-04-26-compile-time-params.md`.
    fn reify_param_attr(&self, global_decl: &ast::GlobalDecl) -> Option<tir::ParamSpec> {
        let attr = global_decl.attributes.iter().find(|a| a.name == PARAM)?;
        let emit = |message: String| self.attr_error(Code::ParamAttr, attr, message);

        let mut ok = true;
        if global_decl.mutable {
            emit("#[param] cannot be applied to a mutable global".to_string());
            ok = false;
        }

        for arg in &attr.args {
            match arg {
                ast::AttrArg::KeyValue(k, _) if k == "name" || k == "from_env" => {}
                ast::AttrArg::KeyValue(k, _)
                | ast::AttrArg::KeyArray(k, _)
                | ast::AttrArg::KeyIdentArray(k, _)
                | ast::AttrArg::KeyIdent(k, _)
                | ast::AttrArg::KeyNumber(k, _) => {
                    emit(format!("unknown #[param] argument: {k}"));
                    ok = false;
                }
                bare
                @ (ast::AttrArg::Str(_) | ast::AttrArg::Ident(_) | ast::AttrArg::Number(_)) => {
                    emit(format!("unknown #[param] argument: {}", bare.name()));
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

    /// What a body-less declaration states with `#[storage]` and
    /// `#[side_effect]`. It owes both, unless it only reserves a name. A
    /// Component Model import states neither: the raw call
    /// its adapter makes carries the facts, the same for every import.
    fn reify_declared_facts(
        &self,
        func: &ast::Function,
        params: &[tir::TirParam],
        return_type: tir::TypeId,
    ) -> Option<BuiltinFacts<String>> {
        if func.body.is_some() || reserves_a_name_only(func) {
            return None;
        }
        let written = func
            .attrs
            .iter()
            .filter(|attr| attr.name == STORAGE || attr.name == SIDE_EFFECT);
        if carries_cm(func) {
            for attr in written {
                self.attr_error(
                    code_of(&attr.name),
                    attr,
                    format!(
                        "#[{}] does not belong on a Component Model import: the raw call \
                         its adapter makes carries the facts, the same for every import",
                        attr.name
                    ),
                );
            }
            return None;
        }
        let type_table = self.tysys.type_table.borrow();
        let shapes: Vec<ParamShape<'_>> = params
            .iter()
            .map(|p| ParamShape::of(&p.name, p.type_id, &type_table))
            .collect();
        let ret = ReturnShape::of(return_type, &type_table);
        match builtin_facts::read(&func.attrs, &shapes, ret) {
            Ok(Some(facts)) => Some(facts),
            Ok(None) => {
                self.error_at(
                    Code::StorageAttr,
                    func.name_span,
                    format!(
                        "`{}` has no body, so it states what a call does: \
                         declare #[storage(...)] and #[side_effect(...)]",
                        func.name
                    ),
                );
                None
            }
            Err(faults) => {
                for fault in faults {
                    let attr = func
                        .attrs
                        .iter()
                        .filter(|attr| attr.name == fault.attr)
                        .nth(fault.occurrence)
                        .expect("a fault points at an attribute the declaration carries");
                    self.attr_error(code_of(fault.attr), attr, fault.message);
                }
                None
            }
        }
    }

    /// The parameters named by `#[immediate(p)]`, each lowered to a Wasm
    /// immediate and so required to still be a literal when codegen reads it.
    fn reify_immediate_attrs(&self, func: &ast::Function, params: &[tir::TirParam]) -> Vec<String> {
        func.attrs
            .iter()
            .filter(|a| a.name == IMMEDIATE)
            .filter_map(|attr| self.reify_immediate_attr(attr, params, carries_cm(func)))
            .collect()
    }

    fn reify_immediate_attr(
        &self,
        attr: &ast::Attribute,
        params: &[tir::TirParam],
        is_cm_import: bool,
    ) -> Option<String> {
        let emit = |message: String| {
            self.attr_error(Code::ImmediateAttr, attr, message);
            None
        };
        if is_cm_import {
            return emit(
                "#[immediate] does not belong on a Component Model import: its adapter \
                 calls it rather than encoding it as one instruction"
                    .to_string(),
            );
        }
        let Some(ast::AttrArg::Ident(name)) = attr.args.first() else {
            return emit("#[immediate] names one parameter, unquoted".to_string());
        };
        if !params.iter().any(|p| &p.name == name) {
            return emit(format!("#[immediate] names no parameter: {name}"));
        }
        if attr.args.len() > 1 {
            return emit(
                "#[immediate] names one parameter; repeat the attribute for another".to_string(),
            );
        }
        Some(name.clone())
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
        let stmts = self.reify_positioned_stmts(
            &block.stmts,
            block.span,
            &mut ctx.enter_scope(),
            expected_type,
            tail_value,
        );
        self.probe_block(
            ProbeSite::BlockStart,
            block.id,
            TirBlock::new(stmts, block.span),
        )
    }

    /// Reify a statement slice at block position. `block_span` is the enclosing
    /// block's span, used for synthetic continuation blocks. A `let ... else`
    /// consumes the *rest* of the slice as its then-arm (see
    /// [`Self::reify_let_else`]), so this is recursive.
    fn reify_positioned_stmts(
        &mut self,
        slice: &[ast::Stmt],
        block_span: Span,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
        tail_value: bool,
    ) -> Vec<TirStmt> {
        let len = slice.len();
        let mut stmts = Vec::new();
        for (i, s) in slice.iter().enumerate() {
            stmts.extend(self.site_stmts(ProbeSite::BeforeStmt, s.id(), s.span()));
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
                    stmts.push(TirStmt::new(TirStmtKind::Expr(expr), expr_stmt.span));
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
                        .unwrap_or(TypeTable::UNKNOWN);
                    let tir = self.reify_match_expr(match_expr, ctx, expected_type, recorded);
                    stmts.push(TirStmt::new(TirStmtKind::Expr(tir), match_expr.span));
                    continue;
                }
                if let ast::Stmt::LabeledBlock(labeled_block) = s {
                    let block = self.reify_block(
                        &labeled_block.block,
                        &mut ctx.enter_labeled_block(labeled_block.label.clone(), expected_type),
                        expected_type,
                    );
                    stmts.push(TirStmt::new(
                        TirStmtKind::LabeledBlock {
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

    /// Mark a block cold ahead of its first statement that cannot return.
    ///
    /// Marking here rather than at each synthesis site is what gives a
    /// `panic(…)` written in source the same treatment as the ones this file
    /// builds. A `return` / `break` ends a path too, but a normal exit is not a
    /// cold one, so it is left alone.
    fn mark_diverging_stmts(&self, stmts: Vec<TirStmt>) -> Vec<TirStmt> {
        let tt = self.tysys.type_table.borrow();
        let diverges =
            |s: &TirStmt| matches!(&s.kind, TirStmtKind::Expr(e) if tt.is_never(e.type_id));
        if !stmts.iter().any(diverges) {
            return stmts;
        }
        let mut out: Vec<TirStmt> = Vec::with_capacity(stmts.len() + 1);
        // A marker makes its whole basic block cold, so one per block is all
        // there is to place, and a synthesis site that placed its own keeps it.
        let mut marked = false;
        for s in stmts {
            marked = marked || is_cold_marker_stmt(&s);
            if diverges(&s) && !marked {
                out.push(known_cold_path_stmt(s.span));
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
        block_span: Span,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
        tail_value: bool,
    ) -> TirStmt {
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
        let else_type = tir::block_result_type(&self.tysys.type_table.borrow(), &else_block);
        let else_span = else_block.span;

        let tir_pattern = self.reify_pattern(&l.else_pattern(), scrutinee_type, span, ctx);

        let cont_stmts =
            self.reify_positioned_stmts(rest, block_span, ctx, expected_type, tail_value);
        let cont_block = TirBlock::new(cont_stmts, block_span);
        let then_type = tir::block_result_type(&self.tysys.type_table.borrow(), &cont_block);

        let match_type = agree_branch_types(&self.tysys.type_table.borrow(), then_type, else_type)
            .unwrap_or(TypeTable::UNIT);
        let then_body = TirExpr::new(TirExprKind::Block(cont_block), then_type, span);
        let else_body = TirExpr::new(TirExprKind::Block(else_block), else_type, else_span);
        let arms = vec![
            TirMatchArm {
                pattern: tir_pattern,
                guard: None,
                body: then_body,
                span,
                consumes: true,
            },
            TirMatchArm {
                pattern: TirPattern::Wildcard,
                guard: None,
                body: else_body,
                span: else_span,
                consumes: false,
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
                // `Continue`.
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
                let tir =
                    self.reify_match_expr(match_expr, ctx, Some(TypeTable::UNIT), TypeTable::UNIT);
                vec![TirStmt::new(TirStmtKind::Expr(tir), match_expr.span)]
            }
            ast::Stmt::Loop(loop_stmt) => {
                let body = self.reify_block(&loop_stmt.body, &mut ctx.enter_loop(), None);
                vec![TirStmt::new(TirStmtKind::Loop { body }, loop_stmt.span)]
            }
            ast::Stmt::LabeledBlock(labeled_block) => {
                // `LABEL: { … }` stmt — mirrors
                // `Elaborator::resolve_labeled_block`. The block result is
                // dropped at stmt position, so no `expected_type` propagates.
                let block = self.reify_block(
                    &labeled_block.block,
                    &mut ctx.enter_labeled_block(labeled_block.label.clone(), None),
                    None,
                );
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
            ast::Stmt::Assert(assert_stmt) if assert_stmt.contract => {
                let check = self.reify_assert(assert_stmt, ctx);
                vec![contract_check(check, assert_stmt.span)]
            }
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
        // Uninitialised `let x: T;` — the parser guarantees `ty`
        // is present, and annotate that the pattern is a single name or `_`.
        // The WIR builder zero-initialises the slot; reify emits a Unit
        // placeholder as the `value` and the `type_id` field carries the
        // user-declared type.
        let Some(ast_value) = let_stmt.value.as_ref() else {
            let placeholder = TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, let_stmt.span);
            let Some(binding) = let_stmt.pattern.as_name() else {
                assert!(
                    matches!(let_stmt.pattern, ast::Pattern::Wildcard),
                    "annotate rejects an uninitialized `let` that destructures"
                );
                return TirStmt::new(TirStmtKind::Expr(placeholder), let_stmt.span);
            };
            let type_id = self
                .ann_local_type(binding.id)
                .expect("annotate records an uninitialized `let`'s type on its local");
            let is_mut = let_stmt.is_mut || binding.is_mut;
            let local_index = ctx.add_local_at(
                binding.name.to_string(),
                type_id,
                is_mut,
                Some(binding.id),
                binding.span,
            );
            return TirStmt::new(
                TirStmtKind::Let {
                    name: binding.name.to_string(),
                    local_index,
                    is_mut,
                    type_id,
                    value: placeholder,
                    storage: LetStorage::Planned,
                },
                let_stmt.span,
            );
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
            }
            | ast::Pattern::MutIdent {
                id,
                name,
                span: binding_span,
            } if self.ann_local_type(*id).is_some() => {
                // `let mut x = …` carries the mutability on `LetStmt`,
                // not on the `Ident` pattern.
                let is_mut =
                    let_stmt.is_mut || matches!(&let_stmt.pattern, ast::Pattern::MutIdent { .. });
                let local_index =
                    ctx.add_local_at(name.clone(), type_id, is_mut, Some(*id), *binding_span);
                TirStmt::new(
                    TirStmtKind::Let {
                        name: name.clone(),
                        local_index,
                        is_mut,
                        type_id,
                        value,
                        storage: LetStorage::Planned,
                    },
                    let_stmt.span,
                )
            }
            ast::Pattern::Wildcard => {
                // `let _ = expr;` discards. Lower as an Expr stmt.
                TirStmt::new(TirStmtKind::Expr(value), let_stmt.span)
            }
            ast::Pattern::Ident { .. }
            | ast::Pattern::MutIdent { .. }
            | ast::Pattern::Tuple(_, _)
            | ast::Pattern::Struct { .. }
            | ast::Pattern::Variant { .. } => {
                // Destructuring `let [a, b] = …;` / `let Point { x, y }
                // = …;` / `let A(x) = …;` or `let A = …;` of a one-case
                // variant. The TIR uses
                // `TirStmtKind::LetDestructure` rather than `Let`. The
                // shared `reify_pattern` adds the sub-pattern bindings
                // to `ctx`; the value's recorded type drives the
                // pattern's per-binding type lookups.
                self.reify_let_destructure(
                    &let_stmt.pattern,
                    type_id,
                    let_stmt.is_mut,
                    value,
                    let_stmt.span,
                    ctx,
                )
            }
            ast::Pattern::Typed { .. } => {
                unreachable!("the parser keeps a `let`'s top-level ascription in `LetStmt::ty`")
            }
            ast::Pattern::Literal(_) | ast::Pattern::Or(_) | ast::Pattern::Range { .. } => {
                // Annotate rejects a refutable `let` pattern.
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

    /// Reify an expression. Reads `sem.types.expression_types` for the
    /// type, `sem.types.coercions` for any coercion wrap,
    /// `sem.types.method_dispatch` for method calls,
    /// `sem.types.desugars` for desugar expansions, and
    /// `sem.types.generic_instantiations` for generic call /
    /// struct-literal / variant-ctor type args. A place the body moves is
    /// marked `Move`, for drop elaboration to take.
    pub(super) fn reify_expr(
        &mut self,
        expr: &ast::Expr,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
    ) -> TirExpr {
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

        let view = self.ann_list_view_conversions(expr.id());
        let tir = self.reify_unmoved(expr, ctx, if view.is_some() { None } else { expected_type });
        let tir = if self.moves(expr) { tir.moved() } else { tir };
        match view {
            Some(view) => {
                let span = tir.span;
                TirExpr::new(
                    TirExprKind::ListViewOf {
                        value: Box::new(tir),
                    },
                    view,
                    span,
                )
            }
            None => tir,
        }
    }

    fn reify_unmoved(
        &mut self,
        expr: &ast::Expr,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
    ) -> TirExpr {
        // The expression's recorded type is the source of truth for
        // `TirExpr::type_id`. Falls back to `expected_type` (or
        // `UNKNOWN` when neither is available) for AST shapes that
        // evaporated during annotate (e.g. a stmt-position match
        // whose recorder fires only at the stmt level). An indefinite one
        // (a bare `null`'s `Option<!>`) holds only where nothing else decides.
        let recorded_type = self
            .ann_expression_types(expr.id())
            .or(expected_type)
            .or_else(|| {
                self.ann_recorded_expression_type(expr.id())
                    .filter(|&t| self.tysys.type_table.borrow().contains_never_arg(t))
            })
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
            ast::Expr::Block(block) => {
                let block_tir = self.reify_block_value(block, ctx, expected_type);
                TirExpr::new(TirExprKind::Block(block_tir), recorded_type, span)
            }
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
                self.reify_composite_literal(tuple_lit.id, recorded_type, span, |this, ty| {
                    this.reify_tuple_literal(tuple_lit, ctx, ty, span)
                })
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
                    .unwrap_or(TypeTable::ERROR);
                let inner = self.reify_expr(&cast.expr, ctx, None);
                let inner =
                    read_through_for_cast(&self.tysys.type_table.borrow(), inner, target_type);
                self.lower_cast(inner, target_type, span)
            }
            ast::Expr::Unary(unary) if unary.op == ast::UnaryOp::Unchecked => {
                let mut call = self.reify_expr(&unary.expr, ctx, expected_type);
                retarget_to_unchecked_twin(&mut call, unary.expr.span());
                call
            }
            ast::Expr::Unary(unary) => {
                // `&mut [1, 2] as List<i32>` parses as `(&mut [1, 2]) as …`,
                // and annotate coerced the literal through the reference and
                // typed the whole unary as the target. Reify drops the wrapper
                // to match: the call site takes a reference to the `List<i32>`, where a
                // `&mut` around it would name the referent instead.
                if matches!(unary.op, ast::UnaryOp::Ref | ast::UnaryOp::MutRef)
                    && let ast::Expr::TupleLiteral(inner) = &unary.expr
                    && self.ann_sequence_coercions(inner.id).is_some()
                {
                    return self.reify_expr(&unary.expr, ctx, expected_type);
                }
                let op = ast_unary_op_to_tir(unary.op);
                let inner_expected = Some(recorded_type)
                    .filter(|&t| t != TypeTable::UNKNOWN && unary_passes_expected_type(unary));
                let inner = self.reify_expr(&unary.expr, ctx, inner_expected);
                if self.tysys.type_table.borrow().is_never(inner.type_id) {
                    return inner;
                }
                if let Some(dispatch) = self.ann_operator_dispatch(unary.id) {
                    // Operator-trait dispatch path for `-x` / `~x` on a
                    // user type (`Neg::neg` / `BitNot::bitnot`). Mirrors the
                    // binary path: a bare `Unary` on a struct operand would
                    // be rejected by codegen (`expected i32, found (ref $T)`),
                    // so replay the recorded method call instead. Unary
                    // operators take no extra arguments.
                    let receiver = self.adjust_receiver(
                        inner,
                        dispatch.self_kind,
                        dispatch.is_ref_impl(),
                        ctx,
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
                if matches!(op, TirUnaryOp::Neg) {
                    let half = self.tysys.half_format(inner.type_id);
                    match &inner.kind {
                        TirExprKind::IntLiteral { value, .. } if let Some(format) = half => {
                            return half_literal(value ^ format.sign_bit(), inner.type_id, span);
                        }
                        TirExprKind::IntLiteral { value, repr } => {
                            return TirExpr::new(
                                TirExprKind::IntLiteral {
                                    value: self.tysys.negated_int_literal(*value, inner.type_id),
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
                        } => {
                            if let TirExprKind::IntLiteral { value, repr } = &cast_inner.kind {
                                let neg_literal = TirExpr::new(
                                    TirExprKind::IntLiteral {
                                        value: self
                                            .tysys
                                            .negated_int_literal(*value, cast_inner.type_id),
                                        repr: format!("-{repr}"),
                                    },
                                    cast_inner.type_id,
                                    span,
                                );
                                return bare_cast(neg_literal, *target_type, span);
                            }
                        }
                        _ => {}
                    }
                }

                // The boxing pass (`lower::plan::boxing`) reads
                // `TirFunction::address_taken_locals` to retag a referenced
                // local's declaration to its box type, so that a write through
                // the reference (`*slot = other_fn`) reaches the original
                // slot. Unrecorded, the local stays unboxed and `&mut local`
                // boxes a throwaway copy.
                if matches!(op, TirUnaryOp::Ref | TirUnaryOp::MutRef)
                    && let TirExprKind::Local { index, .. } = &inner.kind
                {
                    ctx.address_taken_locals.insert(*index);
                }
                // `&*r` / `&mut *r` names the storage `r` already refers to;
                // taking `&` of the deref would box a copy of a scalar.
                if matches!(op, TirUnaryOp::Ref | TirUnaryOp::MutRef)
                    && let TirExprKind::Unary {
                        op: TirUnaryOp::Deref,
                        expr: referent,
                    } = &inner.kind
                    && self.tysys.type_table.borrow().type_key(referent.type_id)
                        == self.tysys.type_table.borrow().type_key(recorded_type)
                {
                    return *referent.clone();
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
            ast::Expr::Match(match_expr) => {
                self.reify_match_expr(match_expr, ctx, expected_type, recorded_type)
            }
            ast::Expr::StructLiteral(struct_lit) => {
                self.reify_struct_literal(struct_lit, ctx, recorded_type)
            }
            ast::Expr::Range(range) => self.reify_range(range, ctx),
            ast::Expr::TemplateString(template) => self.reify_template_string(template, ctx),
            ast::Expr::TaggedTemplate(tagged) => {
                self.reify_tagged_template(tagged, ctx, recorded_type)
            }
            ast::Expr::Matches(m) => self.reify_matches(m, ctx),
            ast::Expr::CompoundAssign(compound) => self.reify_compound_assign(compound, ctx),
            ast::Expr::TryOp(qm) => self.reify_question_mark(qm, ctx),
            ast::Expr::Closure(closure) => self.reify_closure(closure, ctx, expected_type),
            ast::Expr::Index(index) => self.reify_index(index, ctx, recorded_type),
            ast::Expr::ComparisonChain(chain) => self.reify_comparison_chain(chain, ctx),
            ast::Expr::StaticMethodCall(static_call) => {
                self.reify_static_method_call(static_call, ctx, recorded_type)
            }
            ast::Expr::LabeledBlock(lb) => {
                // Annotate unified the breaks into the recorded type, so a `null` that only a
                // sibling break types still coerces where the use site expects nothing.
                let branch_expected = expected_type.or(Some(recorded_type));
                let tir_block = self.reify_block(
                    &lb.block,
                    &mut ctx.enter_labeled_block(lb.label.clone(), branch_expected),
                    branch_expected,
                );
                TirExpr::new(
                    TirExprKind::LabeledBlock {
                        label: lb.label.clone(),
                        block: tir_block,
                        result_type: recorded_type,
                    },
                    recorded_type,
                    span,
                )
            }
            ast::Expr::Spread(_, _) => {
                // `Spread` is only valid inside a tuple literal; the
                // elaborator panics if it sees one at top level.
                // Mirror the panic — annotate would have already
                // diagnosed a stray spread.
                panic!("reify_expr: bare Spread is invalid outside TupleLiteral")
            }
            ast::Expr::If(if_expr) => {
                self.reify_if_expr(if_expr, ctx, expected_type, recorded_type)
            }
            ast::Expr::Assign(assign) => match &assign.target {
                ast::AssignTarget::Place(target) => {
                    self.reify_write(target, &assign.value, span, ctx)
                }
                ast::AssignTarget::Destructuring { places, .. } => {
                    self.reify_destructuring_assign(assign, places, ctx)
                }
            },
            ast::Expr::FieldAccess(field_access) => {
                // The `field_index` and `field_name` on `FieldAccess`
                // TIR are positional; the elaborator looks them up from
                // the receiver's struct decl. Reify reads the same
                // info from `tysys.data.struct_fields` keyed by the
                // receiver's resolved struct name.
                let inner = self.reify_expr(&field_access.expr, ctx, None);
                if self.tysys.type_table.borrow().is_never(inner.type_id) {
                    return inner;
                }
                // A field is read through the one reference that is the
                // struct's handle: any further layer is dereferenced to it,
                // and a newtype over that reference is cast to it.
                let inner = {
                    let tt = self.tysys.type_table.borrow();
                    let inner = if self.tysys.wraps_reference(tt.peel_refs(inner.type_id)) {
                        wrapped_reference(inner, &tt)
                    } else {
                        inner
                    };
                    deref_to_layers(inner, 1, &tt)
                };
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
    /// `resolve_condition_expr` made.
    fn reify_condition_expr(&mut self, expr: &ast::Expr, ctx: &mut FunctionContext) -> TirExpr {
        let cond = self.reify_expr(expr, ctx, Some(TypeTable::BOOL));
        assert!(
            admits_condition(&self.tysys.type_table.borrow(), cond.type_id),
            "`resolve_condition_expr` rejects a non-`bool` condition, and a module holding one is never reified; got {}",
            cond.type_id
        );
        cond
    }

    /// Reify a `while cond { body }` statement as `Loop { if !cond { break; } body }`,
    /// the desugar `DesugarKind::While` tags.
    fn reify_while(&mut self, w: &ast::WhileStmt, ctx: &mut FunctionContext) -> Vec<TirStmt> {
        let span = w.span;
        let mut frame = ctx.enter_loop();
        let ctx = &mut *frame;

        let stmts = match &w.condition {
            ast::Condition::Expr(cond_expr) => {
                let cond_span = cond_expr.span();
                let cond_tir = self.reify_condition_expr(cond_expr, ctx);
                let if_break = break_unless(cond_tir, cond_span, span);
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
                let else_block = TirBlock::new(vec![bare_break(span)], *cond_span);
                self.reify_let_chain_stmts(
                    elements,
                    |r, ctx| r.reify_block_with_position(&w.body, ctx, None, false).stmts,
                    Some(&else_block),
                    &mut ctx.enter_scope(),
                    *cond_span,
                )
            }
        };

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
        expected_type: Option<tir::TypeId>,
    ) -> TirExpr {
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
        // and a binding would keep an aggregate live across the assert. A
        // binding would also copy an affine place the condition only borrows.
        let rereads = is_place && (!conditional || (!self.takes(expr) && self.is_affine(expr)));
        if rereads {
            let cap_ctx = ctx
                .reify_assert_capture_ctx
                .as_mut()
                .expect("reify_assert_capture_ctx survives recursive reify");
            // The failure branch reads the place, which the condition may
            // have moved out of.
            cap_ctx.slots[slot_idx].place_expr = Some(resolved.unmoved().clone());
            if !conditional {
                cap_ctx.slots[slot_idx].emitted = true;
                cap_ctx.slots[slot_idx].type_id = Some(type_id);
                return resolved;
            }
        }

        // `defining_ast_id = None` keeps synthetic locals out of
        // `local_symbols` (LSP hover / go-to-def).
        let local_index =
            (!rereads).then(|| ctx.add_local(cap_name.clone(), type_id, !hoisted, None));
        let local_ref = local_index.map(|index| {
            TirExpr::new(
                TirExprKind::Local {
                    index,
                    name: cap_name.clone(),
                },
                type_id,
                cap_span,
            )
        });
        let seen_local_index = conditional
            .then(|| ctx.add_local(seen_local_name(&cap_name), TypeTable::BOOL, true, None));

        let mut decls = Vec::new();
        let spliced = if hoisted {
            // A hoisted slot binds ahead of the condition, its scope reaching
            // the failure branch too — sound because all that precedes it is
            // bound too.
            assert!(
                seen_local_index.is_none(),
                "a conditional slot is never hoisted"
            );
            let local_ref = local_ref.expect("a hoisted slot is never a place");
            decls.push(TirStmt::new(
                TirStmtKind::Let {
                    name: cap_name,
                    local_index: local_index.expect("bound with its reference"),
                    is_mut: false,
                    type_id,
                    value: resolved,
                    storage: LetStorage::Planned,
                },
                cap_span,
            ));
            // The capture stands where the operand did: what consumed the
            // operand moves the local.
            if self.takes(expr) {
                local_ref.moved()
            } else {
                local_ref
            }
        } else {
            // Otherwise the capture sits where the operand does, moving no
            // evaluation; until it runs, the local holds its Wasm default.
            let mut stmts = Vec::new();
            let tail = match &local_ref {
                // The block ends in the place itself, so it yields what the
                // place holds without taking it.
                None => resolved,
                // The local hands on what the operand evaluated to, which it
                // owns if the value is affine.
                Some(local_ref) => {
                    stmts.push(assign_stmt(local_ref.clone(), resolved, cap_span));
                    if self.is_affine(expr) {
                        local_ref.clone().moved()
                    } else {
                        local_ref.clone()
                    }
                }
            };
            if let Some(seen_index) = seen_local_index {
                let seen_ref = TirExpr::new(
                    TirExprKind::Local {
                        index: seen_index,
                        name: seen_local_name(&cap_name),
                    },
                    TypeTable::BOOL,
                    cap_span,
                );
                decls.push(TirStmt::new(
                    TirStmtKind::Let {
                        name: seen_local_name(&cap_name),
                        local_index: seen_index,
                        is_mut: true,
                        type_id: TypeTable::BOOL,
                        value: bool_lit(false, cap_span),
                        storage: LetStorage::Taken,
                    },
                    cap_span,
                ));
                stmts.push(assign_stmt(seen_ref, bool_lit(true, cap_span), cap_span));
            }
            stmts.push(TirStmt::new(TirStmtKind::Expr(tail), cap_span));
            TirExpr::new(
                TirExprKind::Block(TirBlock::new(stmts, cap_span)),
                type_id,
                cap_span,
            )
        };

        let cap_ctx = ctx
            .reify_assert_capture_ctx
            .as_mut()
            .expect("reify_assert_capture_ctx survives recursive reify");
        cap_ctx.slots[slot_idx].emitted = true;
        cap_ctx.slots[slot_idx].local_index = local_index;
        cap_ctx.slots[slot_idx].type_id = Some(type_id);
        cap_ctx.slots[slot_idx].seen_local_index = seen_local_index;
        cap_ctx.emitted_lets.extend(decls);

        spliced
    }

    /// A conditional slot's failure-message text, chosen in the cold branch so
    /// an unreached slot says so instead of quoting its zero value.
    fn assert_slot_text_let(
        &mut self,
        render_index: u32,
        render_name: String,
        string_type: TypeId,
        seen_index: u32,
        seen_name: String,
        value_ref: TirExpr,
        inspect_spec: Option<String>,
        span: Span,
        ctx: &mut FunctionContext,
    ) -> TirStmt {
        let mut pieces = TemplatePieces::default();
        pieces.hole(value_ref, inspect_spec);
        let rendered = self.reify_string_format(pieces, span, ctx);
        let marker = TirExpr::new(
            TirExprKind::StringLiteral(NOT_EVALUATED.to_string()),
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
                type_id: string_type,
                value: choice,
                storage: LetStorage::Planned,
            },
            span,
        )
    }

    /// What a slot's `value` interpolates as, with its format spec: `Inspect`,
    /// or `<moved>` where a move emptied the place. A variant spends null on a
    /// case of its own, so only a struct or a resource is told apart.
    fn assert_slot_rendering(
        &mut self,
        value: TirExpr,
        string_type: TypeId,
        span: Span,
        ctx: &mut FunctionContext,
    ) -> (TirExpr, Option<String>) {
        let inspect_spec = FormatKind::Inspect.as_char().map(String::from);
        let can_be_moved = self.tysys.carries_affine(value.type_id) && {
            let tt = self.tysys.type_table.borrow();
            let head = tt.representation_head(value.type_id);
            tt.nominal_def(head)
                .is_none_or(|def| tt.variant_template_cases(def).is_none())
        };
        if !can_be_moved {
            return (value, inspect_spec);
        }
        let string = |kind| TirExpr::new(kind, string_type, span);
        let moved = builtin_call("is_moved", vec![value.clone()], TypeTable::BOOL);
        let mut pieces = TemplatePieces::default();
        pieces.hole(value, inspect_spec);
        let inspected = self.reify_string_format(pieces, span, ctx);
        let choice = string(TirExprKind::If {
            condition: Box::new(moved),
            then_branch: TirBlock::new(
                vec![TirStmt::new(
                    TirStmtKind::Expr(string(TirExprKind::StringLiteral(MOVED.to_string()))),
                    span,
                )],
                span,
            ),
            else_branch: Some(TirBlock::new(
                vec![TirStmt::new(TirStmtKind::Expr(inspected), span)],
                span,
            )),
        });
        (choice, None)
    }

    /// Reify `assert cond[, msg];` into the power-assert expansion, from the
    /// slots annotate recorded in [`super::sem::types::AssertCaptureInfo`].
    /// Mirrors [`super::Elaborator::desugar_assert`].
    fn reify_assert(
        &mut self,
        assert_stmt: &ast::AssertStmt,
        ctx: &mut FunctionContext,
    ) -> Vec<TirStmt> {
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

        let mut scope = ctx.enter_scope();
        let ctx = &mut scope.replacing(
            |ctx| &mut ctx.reify_assert_capture_ctx,
            Some(ReifyAssertCaptureContext {
                slots: slot_facts
                    .iter()
                    .enumerate()
                    .map(
                        |(i, (label, conditional, is_place, hoisted))| ReifyAssertSlot {
                            name: format!("$v{i}"),
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
            }),
        );

        let cond_tir = self.reify_condition_expr(&assert_stmt.condition, ctx);

        let actx = ctx
            .reify_assert_capture_ctx
            .take()
            .expect("reify_assert_capture_ctx survives condition reify");

        let mut inner_stmts: Vec<TirStmt> = Vec::with_capacity(actx.emitted_lets.len() + 2);
        inner_stmts.extend(actx.emitted_lets);

        let cond_type = cond_tir.type_id;
        let cond_name = "$cond".to_string();
        let cond_local_index = ctx.add_local(cond_name.clone(), cond_type, false, None);
        inner_stmts.push(TirStmt::new(
            TirStmtKind::Let {
                name: cond_name.clone(),
                local_index: cond_local_index,
                is_mut: false,
                type_id: cond_type,
                value: cond_tir,
                storage: LetStorage::Planned,
            },
            span,
        ));

        let string_type = self
            .tysys
            .type_table
            .borrow_mut()
            .make_compiler_struct(CompilerItem::String);
        // Allocated for every conditional slot, not just the emitted ones, so
        // annotate's index accounting stays in lockstep.
        let render_local_of: IndexMap<usize, u32> = actx
            .slots
            .iter()
            .enumerate()
            .filter(|(_, slot)| slot.conditional)
            .map(|(i, slot)| {
                let name = render_local_name(&slot.name);
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
        let neg_cond = not_expr(cond_ref, span);

        // Header + `condition: <source>` + one `<label>: <text>` line per
        // emitted slot.
        let line = span.line as u64;
        let file = self
            .tysys
            .trait_env
            .parse_modules()
            .module_of_span(span, &self.current_module_source)
            .to_string();
        let mut pieces = TemplatePieces::default();
        if assert_stmt.contract {
            // The call breaking the contract is to blame, so the clause reports
            // where it was called from.
            let clause_site = TirExpr::new(
                TirExprKind::StringLiteral(format!("{file}:{line}")),
                string_type,
                span,
            );
            let owner = contract_owner_name(&ctx.function_name);
            pieces.literal(&format!("Contract of {owner} failed at "));
            pieces.hole(
                builtin_call(CONTRACT_SITE_INTRINSIC, vec![clause_site], string_type),
                None,
            );
        } else {
            let function = &ctx.function_name;
            pieces.literal(&format!("Assertion failed in {function} at {file}:{line}"));
        }
        if let Some(msg) = &assert_stmt.message {
            pieces.literal(": ");
            let msg_tir = self.reify_expr(msg, ctx, None);
            pieces.hole(msg_tir, None);
        }

        let condition_source = info.as_ref().map_or_else(
            || unparse_expr_source(&assert_stmt.condition),
            |info| info.condition_source.clone(),
        );
        pieces.literal(&format!("\ncondition: {condition_source}\n"));

        let mut text_lets: Vec<TirStmt> = Vec::new();
        for (slot_idx, slot) in actx.slots.iter().enumerate() {
            if !slot.emitted {
                continue;
            }
            let Some(type_id) = slot.type_id else {
                continue;
            };
            pieces.literal(&format!("{}: ", slot.label));
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
            let (local_ref, inspect_spec) =
                self.assert_slot_rendering(local_ref, string_type, span, ctx);
            match slot.seen_local_index {
                Some(seen_index) => {
                    let render_index = render_local_of[&slot_idx];
                    let render_name = render_local_name(&slot.name);
                    text_lets.push(self.assert_slot_text_let(
                        render_index,
                        render_name.clone(),
                        string_type,
                        seen_index,
                        seen_local_name(&slot.name),
                        local_ref,
                        inspect_spec,
                        span,
                        ctx,
                    ));
                    let rendered = TirExpr::new(
                        TirExprKind::Local {
                            index: render_index,
                            name: render_name,
                        },
                        string_type,
                        span,
                    );
                    pieces.hole(rendered, None);
                }
                None => pieces.hole(local_ref, inspect_spec),
            }
            pieces.literal("\n");
        }

        let template_tir = self.reify_string_format(pieces, span, ctx);

        // Emit `core:rt::assert_failed`, not `panic`: a distinct callee
        // lets `-f bare-asserts` (see `lower::bare_asserts`) replace assertion
        // failures with a bare trap, dropping this diagnostic without touching
        // explicit `panic(...)` calls. It behaves identically to `panic`.
        let panic_call = rt_call(
            &self.tysys.type_table.borrow(),
            CompilerItem::AssertFailed,
            template_tir,
            TypeTable::NEVER,
            span,
        );

        let mut then_stmts = Vec::with_capacity(text_lets.len() + 2);
        then_stmts.push(known_cold_path_stmt(span));
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

        let assert_serial = ctx.fresh_serial();
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
        match self.ann_desugars(for_of.id) {
            Some(DesugarKind::ForOfTuple) => self.reify_tuple_for_of(for_of, ctx),
            Some(DesugarKind::ForOfVariadic) => self.reify_variadic_for_of(for_of, ctx),
            Some(DesugarKind::ForOfNever) => {
                let (iterable, _) = Elaborator::<H>::split_enumerate(&for_of.iterable);
                let iterable = self.reify_expr(iterable, ctx, None);
                self.skip_for_of_body(for_of.id);
                vec![TirStmt::new(TirStmtKind::Expr(iterable), for_of.span)]
            }
            Some(DesugarKind::ForOfIterator) | None => {
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
            _ => unreachable!("for_of carries one of the ForOf* desugar tags"),
        }
    }

    /// `IntoIterator` path of for-of (extracted from
    /// `reify_for_of` for readability). The `for_of_iterator` record
    /// carries the resolved `into_iter` / `next` `FunctionRef`s.
    fn reify_iterator_for_of(
        &mut self,
        for_of: &ast::ForOfStmt,
        ctx: &mut FunctionContext,
        info: ForOfIteratorInfo,
    ) -> Vec<TirStmt> {
        let span = for_of.span;
        let mut frame = ctx.enter_loop();
        let ctx = &mut *frame;
        let unique_id = ctx.fresh_serial();
        let iter_var = format!("$iter_{unique_id}");
        let label = format!("$for_of_{unique_id}");

        let into_iter_receiver = self.reify_expr(&for_of.iterable, ctx, None);
        let into_iter_receiver = self.adjust_receiver(
            into_iter_receiver,
            info.into_iter_self_kind,
            info.into_iter_is_ref_impl,
            ctx,
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
                type_id: iter_type,
                value: into_iter_call,
                storage: LetStorage::Planned,
            },
            span,
        );

        let ctx = &mut ctx.enter_label(label.clone());

        let iter_local_ref = TirExpr::new(
            TirExprKind::Local {
                index: iter_local_index,
                name: iter_var,
            },
            iter_type,
            span,
        );
        let next_receiver = self.adjust_receiver(
            iter_local_ref,
            info.next_self_kind,
            info.next_is_ref_impl,
            ctx,
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

        let (some_case_name, some_case_index) = {
            let tt = self.tysys.type_table.borrow();
            let (_, _, name, index) = tt.compiler_variant_case(CompilerItem::OptionSome);
            (name.to_string(), index)
        };

        let mut scope = ctx.enter_scope();
        let binding_pattern =
            self.reify_pattern(&for_of.binding, info.item_type, for_of.span, &mut scope);
        let body_block = self.reify_block(&for_of.body, &mut scope, None);

        let some_pattern = TirPattern::Variant {
            enum_type: option_type,
            variant_name: some_case_name,
            case_index: some_case_index,
            bindings: vec![binding_pattern],
            payload_type: info.item_type,
        };

        let body_type = body_block
            .tail_expr()
            .map_or(TypeTable::UNIT, |e| e.type_id);
        let some_body = TirExpr::new(TirExprKind::Block(body_block), body_type, span);

        let break_block = TirBlock::new(vec![bare_break(span)], span);
        let break_body = TirExpr::new(TirExprKind::Block(break_block), TypeTable::UNIT, span);

        let match_type =
            agree_branch_types(&self.tysys.type_table.borrow(), body_type, TypeTable::UNIT)
                .unwrap_or(TypeTable::UNIT);
        let arms = vec![
            TirMatchArm {
                pattern: some_pattern,
                guard: None,
                body: some_body,
                span,
                consumes: true,
            },
            TirMatchArm {
                pattern: TirPattern::Wildcard,
                guard: None,
                body: break_body,
                span,
                consumes: true,
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
        let span = for_of.span;
        let unique_id = ctx.fresh_serial();

        let (actual_iterable, is_enumerate) = Elaborator::<H>::split_enumerate(&for_of.iterable);
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
        if elems.is_empty() {
            self.skip_for_of_body(for_of.id);
        }

        let temp_name = format!("$tuple_{unique_id}");
        let temp_local = ctx.add_local(temp_name.clone(), tuple_type_id, false, None);
        let temp_let = TirStmt::new(
            TirStmtKind::Let {
                name: temp_name.clone(),
                local_index: temp_local,
                is_mut: false,
                type_id: tuple_type_id,
                value: iterable,
                storage: LetStorage::Planned,
            },
            span,
        );

        let mut outer_stmts = vec![temp_let];

        // Annotate recorded one overlay set per instantiation in walk order, and a
        // nested for-of is instantiated once per outer element, so a visit counter picks it.
        let instantiation: &'a [BodyFacts] = {
            let types: &'a TypeAnnotations = self.types;
            let for_of_key = for_of.id;
            let visit = self.tuple_overlay_visits.entry(for_of_key).or_insert(0);
            let k = *visit;
            *visit += 1;
            types
                .tuple_overlays
                .get(&for_of_key)
                .and_then(|insts| insts.get(k))
                .map_or(&[], Vec::as_slice)
        };

        for (i, &elem_type) in elems.iter().enumerate() {
            let ctx = &mut ctx.enter_scope();
            let block_stmts = self.with_overlay(instantiation.get(i), |this| {
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
                let field_access = if by_ref {
                    field_access
                } else {
                    this.handed_on(field_access)
                };

                // By reference (`for v of &tuple`), bind `&T_k` to a fresh copy of
                // the field, matching `for v of &list` refiter semantics.
                let (bind_elem_type, bind_value) = this
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
                    let enum_tuple_type = this
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
                    block_stmts.push(this.reify_let_destructure(
                        &for_of.binding,
                        enum_tuple_type,
                        for_of.is_mut,
                        enum_tuple,
                        span,
                        ctx,
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
                        } if this.ann_local_type(*id).is_some() => {
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
                                    type_id: bind_elem_type,
                                    value: bind_value,
                                    storage: LetStorage::Planned,
                                },
                                span,
                            ));
                        }
                        binding => {
                            block_stmts.push(this.reify_let_destructure(
                                binding,
                                bind_elem_type,
                                for_of.is_mut,
                                bind_value,
                                span,
                                ctx,
                            ));
                        }
                    }
                }

                let body = this.reify_block(&for_of.body, ctx, None);
                block_stmts.extend(body.stmts);
                block_stmts
            });

            outer_stmts.push(TirStmt::new(
                TirStmtKind::LabeledBlock {
                    label: format!("$tuple_iter_{unique_id}_{i}"),
                    block: TirBlock::new(block_stmts, span),
                },
                span,
            ));
        }

        vec![TirStmt::new(
            TirStmtKind::LabeledBlock {
                label: format!("$tuple_for_of_{unique_id}"),
                block: TirBlock::new(outer_stmts, span),
            },
            span,
        )]
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
        let span = for_of.span;
        let (actual_iterable, is_enumerate) = Elaborator::<H>::split_enumerate(&for_of.iterable);
        let iterable = self.reify_expr(actual_iterable, ctx, None);
        let unique_id = ctx.fresh_serial();

        let (bound_type, by_ref) = self
            .tysys
            .type_table
            .borrow_mut()
            .pack_walk_binding(iterable.type_id, ctx.pack_walks)
            .unwrap_or((TypeTable::UNKNOWN, false));
        // Expansion supplies the index literal per unrolled element.
        let binding_type = if is_enumerate {
            self.tysys
                .type_table
                .borrow_mut()
                .make_tuple(vec![TypeTable::I32, bound_type])
        } else {
            bound_type
        };

        if for_of.binding.as_name().is_none()
            && !matches!(
                for_of.binding,
                ast::Pattern::Tuple(..) | ast::Pattern::Wildcard
            )
        {
            return vec![TirStmt::new(TirStmtKind::Expr(iterable), span)];
        }

        let is_mut = for_of.is_mut;
        let ctx = &mut ctx.enter_scope();
        let (binding_name, binding_local, mut destruct_stmts) =
            self.reify_pack_binding(&for_of.binding, binding_type, is_mut, unique_id, span, ctx);

        let index_binding =
            Elaborator::<H>::enumerate_index_local(is_enumerate, &for_of.binding, ctx);
        let mut body = self.reify_block(
            &for_of.body,
            &mut ctx.enter_enumerate_body(index_binding).enter_pack_walk(),
            None,
        );
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
                walk: ctx.pack_walks,
                moves: !by_ref && self.is_affine_type(bound_type),
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
        let span = comp.span;
        let (source, is_enumerate) = Elaborator::<H>::split_enumerate(&comp.iterable);
        let iterable = self.reify_expr(source, ctx, None);
        let unique_id = ctx.fresh_serial();

        let elem_type = self
            .tysys
            .comprehension_pack_elem(iterable.type_id, ctx.pack_walks)
            .unwrap_or(TypeTable::UNKNOWN);
        let binding_type = if is_enumerate {
            self.tysys
                .type_table
                .borrow_mut()
                .make_tuple(vec![TypeTable::I32, elem_type])
        } else {
            elem_type
        };

        let ctx = &mut ctx.enter_scope();
        let (binding_name, binding_local, destructure) =
            self.reify_pack_binding(&comp.binding, binding_type, false, unique_id, span, ctx);

        let index_binding =
            Elaborator::<H>::enumerate_index_local(is_enumerate, &comp.binding, ctx);
        let body = self.reify_expr(
            &comp.body,
            &mut ctx.enter_enumerate_body(index_binding).enter_pack_walk(),
            None,
        );

        TirExpr::new(
            TirExprKind::VariadicTupleComprehension {
                iterable: Box::new(iterable),
                binding_name,
                binding_local,
                destructure,
                body: Box::new(body),
                unique_id,
                is_enumerate,
                walk: ctx.pack_walks,
            },
            recorded_type,
            span,
        )
    }

    /// Bind a type-pack walk's binding: its name, or a minted temp that each
    /// name of a tuple binding reads its element from. Answers the bound name,
    /// its local, and the `Let`s that destructure the temp.
    fn reify_pack_binding(
        &mut self,
        binding: &ast::Pattern,
        binding_type: TypeId,
        is_mut: bool,
        unique_id: u32,
        span: Span,
        ctx: &mut FunctionContext,
    ) -> (String, u32, Vec<TirStmt>) {
        let root = binding.as_name();
        let binding_name = root.map_or_else(
            || minted_name("pattern_temp", unique_id),
            |n| n.name.to_string(),
        );
        let binding_local = ctx.add_local_at(
            binding_name.clone(),
            binding_type,
            is_mut || root.is_some_and(|n| n.is_mut),
            root.map(|n| n.id),
            root.map_or_else(Span::default, |n| n.span),
        );
        let ast::Pattern::Tuple(elems, _) = binding else {
            return (binding_name, binding_local, Vec::new());
        };
        let held = self
            .tysys
            .type_table
            .borrow()
            .elem_types_or_self(binding_type);
        let mut destructure = Vec::new();
        for (i, (elem, elem_type)) in elems.iter().zip(held).enumerate() {
            let Some(elem_name) = elem.as_name() else {
                continue;
            };
            let name = elem_name.name.to_string();
            let is_mut = is_mut || elem_name.is_mut;
            let local_index = ctx.add_local_at(
                name.clone(),
                elem_type,
                is_mut,
                Some(elem_name.id),
                elem_name.span,
            );
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
            destructure.push(TirStmt::new(
                TirStmtKind::Let {
                    name: name.clone(),
                    local_index,
                    is_mut,
                    type_id: elem_type,
                    value: field_access,
                    storage: LetStorage::Planned,
                },
                span,
            ));
        }
        (binding_name, binding_local, destructure)
    }

    /// Reify a C-style `for init; cond; update { body }` loop into
    /// the shape `Elaborator::resolve_for` produces.
    fn reify_for(&mut self, f: &ast::ForStmt, ctx: &mut FunctionContext) -> Vec<TirStmt> {
        let span = f.span;
        let body_label = for_body_label(ctx.fresh_serial());

        let mut frame = ctx.enter_loop();
        let ctx = &mut frame.enter_scope();

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
                let cond_tir = self.reify_condition_expr(cond_expr, ctx);
                let if_break = break_unless(cond_tir, cond_span, span);
                let labeled_body = self.reify_for_labeled_body(&body_label, &f.body, ctx);
                let mut s = vec![if_break, labeled_body];
                s.extend(self.reify_for_update(f.update.as_ref(), ctx));
                s
            }
            Some(ast::Condition::LetChain {
                elements,
                span: cond_span,
            }) => {
                let else_block = TirBlock::new(vec![bare_break(span)], *cond_span);
                self.reify_let_chain_stmts(
                    elements,
                    |r, ctx| {
                        let mut s = vec![r.reify_for_labeled_body(&body_label, &f.body, ctx)];
                        s.extend(r.reify_for_update(f.update.as_ref(), ctx));
                        s
                    },
                    Some(&else_block),
                    &mut ctx.enter_scope(),
                    *cond_span,
                )
            }
        };

        outer_stmts.push(TirStmt::new(
            TirStmtKind::Loop {
                body: TirBlock::new(iter_stmts, span),
            },
            span,
        ));

        outer_stmts
    }

    /// Reify the for-loop body wrapped in `$for_N_body:` so naked
    /// `continue` lowers as `break $for_N_body` (letting the
    /// `update` expression run before the next iteration). Mirrors
    /// `Elaborator::resolve_for_labeled_body`.
    fn reify_for_labeled_body(
        &mut self,
        body_label: &str,
        body: &ast::Block,
        ctx: &mut FunctionContext,
    ) -> TirStmt {
        let mut retarget = ctx.pushing(|ctx| &mut ctx.for_continue_labels, body_label.to_string());
        let body_block = self.reify_block(
            body,
            &mut retarget.enter_label(body_label.to_string()),
            None,
        );
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
                vec![TirStmt::new(TirStmtKind::Expr(tir), u.span())]
            })
            .unwrap_or_default()
    }

    /// Reify a let chain into nested Match / If stmts, shared by
    /// `reify_if_expr` / `reify_if_stmt` / `reify_while` / `reify_for`. A `Let`
    /// element becomes a two-arm Match (pattern arm recurses, wildcard falls
    /// back to `else_block`), an `Expr` element a single-branch `If`; the
    /// recursion bottoms out on the empty list by reifying the success path
    /// with `then`.
    fn reify_let_chain_stmts(
        &mut self,
        elements: &[ast::ConditionElement],
        then: impl FnOnce(&mut Self, &mut FunctionContext) -> Vec<TirStmt>,
        else_block: Option<&TirBlock>,
        ctx: &mut FunctionContext,
        span: Span,
    ) -> Vec<TirStmt> {
        if elements.is_empty() {
            return then(self, ctx);
        }

        match &elements[0] {
            ast::ConditionElement::Let {
                pattern,
                expr,
                span: elem_span,
            } => {
                let scrutinee = self.reify_expr(expr, ctx, None);
                let scrutinee = self.probe_expr(ProbeSite::Around, expr.id(), scrutinee);
                let scrutinee_type = scrutinee.type_id;
                let tir_pattern = self.reify_pattern(pattern, scrutinee_type, *elem_span, ctx);
                let inner_stmts =
                    self.reify_let_chain_stmts(&elements[1..], then, else_block, ctx, span);
                let inner_block = TirBlock::new(inner_stmts, span);
                // Use the shared `block_result_type` so a then/else
                // block ending in a value `If` / `Match` / nested chain
                // contributes its real result type. A hand-rolled
                // "last stmt is Expr" check would mis-classify those
                // trailing forms as `Unit`, collapsing the Match's
                // `match_type` to `Unit` and dropping the branch values.
                let tt = self.tysys.type_table.borrow();
                let then_type = tir::block_result_type(&tt, &inner_block);
                let else_tir = else_block.cloned();
                let else_type = else_tir
                    .as_ref()
                    .map_or(TypeTable::UNIT, |b| tir::block_result_type(&tt, b));
                let else_arm_span = else_tir.as_ref().map_or(span, |b| b.span);
                let match_type =
                    agree_branch_types(&tt, then_type, else_type).unwrap_or(TypeTable::UNIT);
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
                        consumes: true,
                    },
                    TirMatchArm {
                        pattern: TirPattern::Wildcard,
                        guard: None,
                        body: else_body,
                        span: else_arm_span,
                        consumes: false,
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
                let condition = self.reify_condition_expr(expr, ctx);
                let condition = self.probe_expr(ProbeSite::Around, expr.id(), condition);
                let inner_stmts =
                    self.reify_let_chain_stmts(&elements[1..], then, else_block, ctx, span);
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
        match &if_stmt.condition {
            ast::Condition::LetChain { elements, .. } => {
                let else_block = self.else_branch(
                    if_stmt.else_block.as_ref(),
                    if_stmt.id,
                    if_stmt.span,
                    |r, b| r.reify_block_with_position(b, ctx, expected_type, tail_value),
                );
                self.reify_let_chain_stmts(
                    elements,
                    |r, ctx| {
                        r.reify_block_with_position(
                            &if_stmt.then_block,
                            ctx,
                            expected_type,
                            tail_value,
                        )
                        .stmts
                    },
                    else_block.as_ref(),
                    &mut ctx.enter_scope(),
                    if_stmt.span,
                )
            }
            ast::Condition::Expr(cond_expr) => {
                let condition = self.reify_condition_expr(cond_expr, ctx);
                let then_branch = self.reify_block_with_position(
                    &if_stmt.then_block,
                    ctx,
                    expected_type,
                    tail_value,
                );
                let else_branch = self.else_branch(
                    if_stmt.else_block.as_ref(),
                    if_stmt.id,
                    if_stmt.span,
                    |r, b| r.reify_block_with_position(b, ctx, expected_type, tail_value),
                );
                let tt = self.tysys.type_table.borrow();
                let then_type = tir::block_result_type(&tt, &then_branch);
                let else_type = else_branch
                    .as_ref()
                    .map_or(TypeTable::UNIT, |b| tir::block_result_type(&tt, b));
                let result_type =
                    agree_branch_types(&tt, then_type, else_type).unwrap_or(TypeTable::UNIT);
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
        match &if_stmt.condition {
            ast::Condition::Expr(cond_expr) => {
                let condition = self.reify_condition_expr(cond_expr, ctx);
                let then_block = self.reify_block(&if_stmt.then_block, ctx, None);
                let else_block = self.else_branch(
                    if_stmt.else_block.as_ref(),
                    if_stmt.id,
                    if_stmt.span,
                    |r, b| r.reify_block(b, ctx, None),
                );
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
                let else_block = self.else_branch(
                    if_stmt.else_block.as_ref(),
                    if_stmt.id,
                    if_stmt.span,
                    |r, b| r.reify_block(b, ctx, None),
                );
                self.reify_let_chain_stmts(
                    elements,
                    |r, ctx| r.reify_block(&if_stmt.then_block, ctx, None).stmts,
                    else_block.as_ref(),
                    &mut ctx.enter_scope(),
                    if_stmt.span,
                )
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
                let else_block = self.else_branch(
                    if_expr.else_block.as_ref(),
                    if_expr.id,
                    if_expr.span,
                    |r, b| r.reify_block_value(b, ctx, branch_expected),
                );
                let stmts = self.reify_let_chain_stmts(
                    elements,
                    |r, ctx| {
                        r.reify_block_with_position(&if_expr.then_block, ctx, branch_expected, true)
                            .stmts
                    },
                    else_block.as_ref(),
                    &mut ctx.enter_scope(),
                    if_expr.span,
                );
                let chain_block = TirBlock::new(stmts, if_expr.span);
                return TirExpr::new(TirExprKind::Block(chain_block), recorded_type, if_expr.span);
            }
        };
        let condition = self.reify_condition_expr(cond_expr, ctx);
        let then_branch = self.reify_block_value(&if_expr.then_block, ctx, branch_expected);
        let else_branch = self.else_branch(
            if_expr.else_block.as_ref(),
            if_expr.id,
            if_expr.span,
            |r, b| r.reify_block_value(b, ctx, branch_expected),
        );
        TirExpr::new(
            TirExprKind::If {
                condition: Box::new(condition),
                then_branch,
                else_branch,
            },
            recorded_type,
            if_expr.span,
        )
    }

    /// The trait-method call `left.method(right)` an operator dispatched to, each
    /// side adjusted as the method declares it.
    fn binary_operator_call(
        &mut self,
        dispatch: OperatorDispatch,
        left: TirExpr,
        right: TirExpr,
        span: Span,
    ) -> TirExpr {
        assert!(
            !matches!(dispatch.self_kind, ast::SelfKind::MutRef),
            "a binary operator trait takes `&self`, so its receiver needs no box"
        );
        // An impl for a reference answers a newtype over one with the
        // reference it wraps.
        let (left, right) = if dispatch.is_ref_impl() {
            let tt = self.tysys.type_table.borrow();
            let unwrap = |operand: TirExpr| {
                if self.tysys.wraps_reference(operand.type_id) {
                    wrapped_reference(operand, &tt)
                } else {
                    operand
                }
            };
            (unwrap(left), unwrap(right))
        } else {
            (left, right)
        };
        let receiver = adjust_receiver_for_self_kind(
            left,
            dispatch.self_kind,
            dispatch.is_ref_impl(),
            &self.tysys.type_table,
        );
        let call_args: Vec<CallArg> = std::iter::once(right)
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
                        span,
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
            span,
        )
    }

    /// Reify a binary expression, as the trait-method call annotate dispatched it
    /// to or as the native operator.
    fn reify_binary(
        &mut self,
        binary: &ast::BinaryExpr,
        ctx: &mut FunctionContext,
        recorded_type: TypeId,
    ) -> TirExpr {
        // A literal operand takes the other operand's type: an inlined
        // `f32::INFINITY = 1.0 / 0.0` records none for its literals.
        let left_is_lit = is_literal_arithmetic(&binary.left);
        let right_is_lit = is_literal_arithmetic(&binary.right);
        let (left, right) = if left_is_lit && !right_is_lit {
            let right = self.reify_expr(&binary.right, ctx, None);
            let coerce =
                is_primitive_literal_target(&self.tysys.type_table.borrow(), right.type_id)
                    .then_some(right.type_id);
            let left = self.reify_expr(&binary.left, ctx, coerce);
            (left, right)
        } else if right_is_lit && !left_is_lit {
            let left = self.reify_expr(&binary.left, ctx, None);
            let coerce = is_primitive_literal_target(&self.tysys.type_table.borrow(), left.type_id)
                .then_some(left.type_id);
            let right = self.reify_expr(&binary.right, ctx, coerce);
            (left, right)
        } else if left_is_lit && right_is_lit {
            // Both literals: the expression's recorded type is the hint (e.g.
            // the `const_ty` flowing in from a reified const body), on the same
            // terms the elaborator applied it.
            let context = (recorded_type != TypeTable::UNKNOWN).then_some(recorded_type);
            let order = numeric_literal_pair_order(
                &self.tysys.type_table.borrow(),
                &binary.left,
                &binary.right,
                context,
            );
            order.resolve(
                &binary.left,
                &binary.right,
                |expr, hint| self.reify_expr(expr, ctx, hint),
                |expr| expr.type_id,
            )
        } else if binary.op.is_comparison()
            && Elaborator::<H>::takes_shape_from_expected_type(&binary.left)
            && !Elaborator::<H>::takes_shape_from_expected_type(&binary.right)
        {
            let right = self.reify_expr(&binary.right, ctx, None);
            let left = self.reify_expr(&binary.left, ctx, None);
            (left, right)
        } else {
            let left = self.reify_expr(&binary.left, ctx, None);
            let right = self.reify_expr(&binary.right, ctx, None);
            (left, right)
        };
        let right = self.probe_expr(ProbeSite::Around, binary.right.id(), right);
        self.reify_binary_op(
            binary.id,
            binary.op,
            left,
            right,
            recorded_type,
            binary.span,
        )
    }

    /// `left OP right` for the operator node `id`: the trait call the
    /// elaborator recorded for it, else the native operation.
    fn reify_binary_op(
        &mut self,
        id: ast::AstId,
        op: ast::BinaryOp,
        left: TirExpr,
        right: TirExpr,
        recorded_type: TypeId,
        span: Span,
    ) -> TirExpr {
        if self.tysys.compares_handles(op, left.type_id, right.type_id) {
            return handle_comparison(op, left, right, span);
        }

        if let Some(dispatch) = self.ann_operator_dispatch(id) {
            let call = self.binary_operator_call(dispatch, left, right, span);
            // `!=` negates `eq`, and an ordering operator reads `cmp`'s
            // `Ordering`.
            return match op {
                ast::BinaryOp::NotEq if call.type_id == TypeTable::BOOL => not_expr(call, span),
                ast::BinaryOp::Lt
                | ast::BinaryOp::Gt
                | ast::BinaryOp::LtEq
                | ast::BinaryOp::GtEq => {
                    assert_eq!(
                        call.type_id,
                        self.tysys
                            .type_table
                            .borrow_mut()
                            .make_compiler_enum(CompilerItem::Ordering),
                        "an ordering operator dispatches to `cmp`"
                    );
                    ord_bool_from_cmp(call, op, span, &self.tysys.type_table)
                }
                _ => call,
            };
        }

        TirExpr::new(
            TirExprKind::Binary {
                left: Box::new(left),
                op: ast_binary_op_to_tir(op),
                right: Box::new(right),
            },
            recorded_type,
            span,
        )
    }

    /// Reify an untagged template as the `String::format` call it is sugar for.
    fn reify_template_string(
        &mut self,
        template: &ast::TemplateStringExpr,
        ctx: &mut FunctionContext,
    ) -> TirExpr {
        let mut pieces = TemplatePieces::default();
        for part in &template.parts {
            match part {
                // Annotate validated every segment's escapes.
                ast::TemplatePart::String(raw) => {
                    pieces.text(&unescape_template_segment(raw), raw);
                }
                ast::TemplatePart::Interpolation { expr, format, .. } => {
                    let value = self.reify_expr(expr, ctx, None);
                    pieces.hole(value, format.as_ref().map(|f| f.spec.clone()));
                }
            }
        }
        self.reify_string_format(pieces, template.span, ctx)
    }

    /// `` String::format`…` `` over `pieces`, resolved here rather than by
    /// name, so it reaches the prelude's `String` wherever an import or
    /// `#![no_prelude]` takes the name. A template with no hole is its text,
    /// and one that is a lone `String` hole is that value.
    fn reify_string_format(
        &mut self,
        pieces: TemplatePieces,
        span: Span,
        ctx: &mut FunctionContext,
    ) -> TirExpr {
        let TemplatePieces {
            mut segments,
            mut holes,
        } = pieces;
        let string_type = self
            .tysys
            .type_table
            .borrow_mut()
            .make_compiler_struct(CompilerItem::String);
        if holes.is_empty() {
            let [cooked, _] = segments
                .pop()
                .expect("a template has one segment more than holes");
            return TirExpr::new(TirExprKind::StringLiteral(cooked), string_type, span);
        }
        let lone_string = {
            let tt = self.tysys.type_table.borrow();
            matches!(holes.as_slice(), [(value, None)]
                if tt.type_key(value.type_id) == tt.type_key(string_type))
                && segments
                    .iter()
                    .all(|[cooked, raw]| cooked.is_empty() && raw.is_empty())
        };
        if lone_string {
            return holes.pop().expect("matched one hole").0;
        }

        let (values, holes): (Vec<TirExpr>, Vec<TemplateHole>) = holes
            .into_iter()
            .map(|(value, spec)| {
                let ty = value.type_id;
                (value, TemplateHole { ty, spec })
            })
            .unzip();
        let shape = TemplateShape { holes };
        let template_ty = self.reify_template_type(shape, span);
        let (literal, holes) =
            self.reify_template_literal(template_ty, values, &segments, span, ctx);
        let (func, type_args) = {
            let mut tt = self.tysys.type_table.borrow_mut();
            let mut func = method_ref(&tt, CompilerItem::StringFormat);
            func.template = Some(
                tt.compiler_items()
                    .require_template(CompilerItem::StringFormat),
            );
            let hole_types = tt
                .template_shape_of_type(template_ty)
                .expect("a template type has a shape")
                .hole_types();
            (
                func,
                vec![template_ty, tt.template_holes_tuple(&hole_types)],
            )
        };
        let call = TirExpr::new(
            TirExprKind::Call {
                type_args,
                func: Box::new(func),
                args: CallArgs::free(vec![CallArg::new(literal, false)]),
            },
            string_type,
            span,
        );
        Self::bind_holes_around(call, holes, string_type, span)
    }

    /// The anonymous type `shape` denotes, minted on its first sighting in this
    /// module. Annotate mints a tagged template's; this mints the shapes only
    /// reify sees, whose hole types it alone settles.
    fn reify_template_type(&mut self, shape: TemplateShape, span: Span) -> TypeId {
        let mut tt = self.tysys.type_table.borrow_mut();
        let id = tt.intern_template_shape(self.emitted_module_source.clone(), shape);
        let def = StructDef::Anon(id);
        if let Some(ty) = tt.find_struct_type(def) {
            return ty;
        }
        let name = tt.anon_struct_mangle(id);
        let fields = tt.anon_shape_struct_fields(id);
        let ty = tt.make_struct(def);
        self.pending_template_structs.push(TirStruct::anonymous(
            id,
            name,
            self.emitted_module_source.clone(),
            &fields,
            span,
        ));
        ty
    }

    /// Reify a tagged template as the call annotate resolved it: the tag on a
    /// literal of the template's anonymous type (WEP 2026-01-10).
    fn reify_tagged_template(
        &mut self,
        tagged: &ast::TaggedTemplateExpr,
        ctx: &mut FunctionContext,
        recorded_type: TypeId,
    ) -> TirExpr {
        let span = tagged.span;
        let (Some(template_ty), Some(dispatch)) = (
            self.ann_tagged_template(tagged.id),
            self.ann_static_method_dispatch(tagged.id),
        ) else {
            // Annotate diagnosed the template or the tag.
            return TirExpr::new(TirExprKind::Unit, TypeTable::ERROR, span);
        };
        let values: Vec<TirExpr> = tagged
            .template
            .interpolations()
            .map(|expr| self.reify_expr(expr, ctx, None))
            .collect();
        // Annotate validated every segment's escapes.
        let segments: Vec<[String; 2]> = tagged
            .template
            .raw_segments()
            .into_iter()
            .map(|raw| [unescape_template_segment(&raw), raw])
            .collect();
        let (literal, holes) =
            self.reify_template_literal(template_ty, values, &segments, span, ctx);

        // The template is the tag's one written argument; a trailing parameter
        // with a default is filled here, as it is for a spelled call.
        let mut args = vec![CallArg::new(literal, false)];
        let prelude = self.reify_pad_dispatch_defaults(
            &tagged.tag,
            &mut args,
            &dispatch,
            tagged.id,
            span,
            ctx,
        );
        let call = TirExpr::new(
            TirExprKind::Call {
                type_args: dispatch.type_args,
                func: Box::new(dispatch.function_ref),
                args: CallArgs::free(args),
            },
            recorded_type,
            span,
        );
        let call = Self::hoist_block(call, prelude);
        Self::bind_holes_around(call, holes, recorded_type, span)
    }

    /// The literal of `template_ty` over the reified holes and each segment's
    /// cooked and raw text, and the statements binding the holes it cannot
    /// read where it stands: a place that a later computed hole may write.
    fn reify_template_literal(
        &mut self,
        template_ty: TypeId,
        values: Vec<TirExpr>,
        segments: &[[String; 2]],
        span: Span,
        ctx: &mut FunctionContext,
    ) -> (TirExpr, HoleBindings) {
        let (struct_name, shape, open) = {
            let tt = self.tysys.type_table.borrow();
            let ResolvedType::Struct {
                def: def @ StructDef::Anon(id),
                ..
            } = tt.get(template_ty)
            else {
                panic!("annotate records an anonymous struct type for a tagged template");
            };
            let shape = tt
                .template_shape_of_type(template_ty)
                .expect("a template type has a shape")
                .clone();
            // The name the shape was registered under, not the one a message
            // shows: both render the shape, and only the mangle is a key.
            (tt.struct_head_name(*def), shape, tt.anon_shape_is_open(*id))
        };
        let hole_types = shape.hole_types();
        assert_eq!(values.len(), hole_types.len());
        assert_eq!(segments.len(), hole_types.len() + 1);

        let serial = ctx.fresh_serial();
        let mut stmts: Vec<TirStmt> = Vec::new();
        let mut fields: Vec<TirStructField> = Vec::new();
        // The literal evaluates its fields in order, and a hole that computes
        // its value stays a temporary of the statement, dropped as it ends. The
        // template shares a place's storage rather than copying it, so a place
        // a later computed hole may write is copied at its own position. An
        // affine place cannot be copied, and is read in place.
        let last_computed = values.iter().rposition(|v| !Self::is_place_hole(v));
        for (k, (mut value, &hole_ty)) in values.into_iter().zip(&hole_types).enumerate() {
            let snapshot = last_computed.is_some_and(|last| k < last)
                && Self::is_place_hole(&value)
                && !self.is_affine_type(hole_ty);
            if snapshot {
                let name = template_hole_local(serial, k);
                let local_index = ctx.add_local(name.clone(), hole_ty, false, None);
                let hole_span = value.span;
                stmts.push(TirStmt::new(
                    TirStmtKind::Let {
                        name: name.clone(),
                        local_index,
                        is_mut: false,
                        type_id: hole_ty,
                        value,
                        storage: LetStorage::Planned,
                    },
                    hole_span,
                ));
                value = TirExpr::new(
                    TirExprKind::Local {
                        index: local_index,
                        name,
                    },
                    hole_ty,
                    hole_span,
                );
            }
            let mut field = TirStructField {
                name: TemplateShape::field_name(k),
                value,
                field_index: k as u32,
            };
            if !open
                && let Some(taken) =
                    hold_hole(&mut field, hole_ty, &mut self.tysys.type_table.borrow_mut())
            {
                ctx.address_taken_locals.insert(taken);
            }
            fields.push(field);
        }
        let string_type = self
            .tysys
            .type_table
            .borrow_mut()
            .make_compiler_struct(CompilerItem::String);
        for key in shape.segment_keys() {
            let (k, raw) = TemplateShape::segment_of(key);
            let (name, field_index) = shape.segment_field(key);
            fields.push(TirStructField {
                name,
                value: TirExpr::new(
                    TirExprKind::StringLiteral(segments[k][usize::from(raw)].clone()),
                    string_type,
                    span,
                ),
                field_index,
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
        (
            literal,
            HoleBindings {
                lets: stmts,
                serial,
            },
        )
    }

    /// `call` after the statements binding its template's holes, in a block of
    /// its own where there are any.
    fn bind_holes_around(
        call: TirExpr,
        holes: HoleBindings,
        result_type: TypeId,
        span: Span,
    ) -> TirExpr {
        let HoleBindings {
            lets: mut stmts,
            serial,
        } = holes;
        if stmts.is_empty() {
            return call;
        }
        let label = template_block_label(serial);
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
                result_type,
            },
            result_type,
            span,
        )
    }

    /// Whether a reified hole names storage the template literal can read or
    /// reference in situ: a local, a parameter, a global, or a field or element
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
    fn reify_range(&mut self, range: &ast::RangeExpr, ctx: &mut FunctionContext) -> TirExpr {
        // Annotate has unified the endpoints, so either type is the element
        // type — but only in the order the elaborator resolved them in.
        let order = range_endpoint_order(range, self.ann_range_element_hint(range.id));
        let (start, end) = order.resolve(
            &range.start,
            &range.end,
            |expr, hint| self.reify_expr(expr, ctx, hint),
            |expr| expr.type_id,
        );
        let element_type = start.type_id;

        let (_, struct_type) = self.tysys.range_type(range.kind, element_type);

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
                value: bool_lit(false, range.span),
                field_index: 2,
            });
        }

        // `resolve_range` recorded the mangled name (`RangeExclusive<i32>`) on
        // every path that logs no error, and reify walks only clean modules.
        let mangled_name = self
            .ann_generic_instantiations(range.id)
            .and_then(|gi| gi.mangled_name)
            .expect("resolve_range records a range's instantiation");

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

    /// Reify a named struct literal `S { … }`.
    fn reify_struct_literal(
        &mut self,
        struct_lit: &ast::StructLiteralExpr,
        ctx: &mut FunctionContext,
        recorded_type: TypeId,
    ) -> TirExpr {
        self.reify_composite_literal(struct_lit.id, recorded_type, struct_lit.span, |this, ty| {
            this.reify_struct_literal_as(struct_lit, ctx, ty)
        })
    }

    /// Build a literal as `recorded_type`, or, where annotate coerced it to a
    /// newtype over a composite, as that newtype's base retagged.
    fn reify_composite_literal(
        &mut self,
        id: AstId,
        recorded_type: TypeId,
        span: Span,
        build: impl FnOnce(&mut Self, TypeId) -> TirExpr,
    ) -> TirExpr {
        let Some(newtype) = self
            .ann_coercions(id)
            .filter(|choice| choice.kind == CoercionKind::CompositeNewtype)
            .map(|choice| choice.target_type)
        else {
            return build(self, recorded_type);
        };
        let base = self.tysys.type_table.borrow().representation_head(newtype);
        let built = build(self, base);
        cast_to_newtype(built, Some(newtype), span)
    }

    fn reify_struct_literal_as(
        &mut self,
        struct_lit: &ast::StructLiteralExpr,
        ctx: &mut FunctionContext,
        recorded_type: TypeId,
    ) -> TirExpr {
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
            peel_to_struct(&self.tysys.type_table.borrow(), recorded_type).map(|(head, _)| head);

        // What the literal *is*, which annotate recorded on the type — not
        // how it was spelled. Dispatching on the spelling made reify disagree
        // with the pass that decided.
        if struct_lit.name.is_none() && !matches!(struct_head, Some(StructDef::Decl(_))) {
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
            Some(StructDef::Decl(def)) => self.tysys.resolutions.defs().module(def).clone(),
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
        let field_names_to_index: hashmap::IndexMap<String, (u32, TypeId)> = decl_fields
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
            let subst: hashmap::IndexMap<u32, TypeId> = (0..generic_args.len() as u32)
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
                    .unwrap_or((0, TypeTable::UNKNOWN));
                let expected_field_ty = substitute(self, raw_ty);
                let value = self.reify_expr(&f.value, ctx, Some(expected_field_ty));
                TirStructField {
                    name: f.name.clone(),
                    value,
                    field_index,
                }
            })
            .collect();

        let provided: hashmap::IndexSet<String> =
            struct_lit.fields.iter().map(|f| f.name.clone()).collect();

        // The members evaluate in source order, `base` first, but the literal
        // holds its fields by declaration index. A constant observes no order,
        // so only the other fields count: those up to the last one out of
        // declaration order are bound ahead of the literal, a local read like
        // any other, since a later field may write the local. So is `base`,
        // which the literal reads at each omitted field's index, once any
        // field could write it.
        let constant = |this: &Self, e: &TirExpr| {
            tir::is_constant_initializer(e, &this.tysys.type_table.borrow())
        };
        let mut base_binding = Vec::new();
        let base_ref = struct_lit.spreads.first().map(|spread| {
            let base_expr = self.reify_expr(&spread.expr, ctx, Some(struct_type));
            if fields.iter().all(|f| constant(self, &f.value)) {
                return Self::hoist_once(ctx, base_expr, "base", &mut base_binding);
            }
            let name = minted_name("base", ctx.fresh_serial());
            bind_to_local(ctx, name, base_expr, &mut base_binding)
        });
        let ordered: Vec<usize> = (0..fields.len())
            .filter(|&i| !constant(self, &fields[i].value))
            .collect();
        let in_order_from = (1..ordered.len())
            .rev()
            .find(|&k| fields[ordered[k - 1]].field_index > fields[ordered[k]].field_index)
            .map_or(0, |k| ordered[k]);
        fields = fields
            .into_iter()
            .enumerate()
            .map(|(i, f)| {
                if i >= in_order_from || constant(self, &f.value) {
                    return f;
                }
                let name = minted_name("field", ctx.fresh_serial());
                TirStructField {
                    value: self.handed_on(bind_to_local(ctx, name, f.value, &mut base_binding)),
                    ..f
                }
            })
            .collect();
        if let Some(base_ref) = base_ref {
            for (name, field_index, raw_ty, _default) in &decl_fields {
                if provided.contains(name) {
                    continue;
                }
                let field_ty = substitute(self, *raw_ty);
                fields.push(TirStructField {
                    name: name.clone(),
                    value: self.handed_on(TirExpr::new(
                        TirExprKind::FieldAccess {
                            expr: Box::new(base_ref.clone()),
                            field_index: *field_index,
                            field_name: name.clone(),
                        },
                        field_ty,
                        struct_lit.span,
                    )),
                    field_index: *field_index,
                });
            }
        } else {
            // This literal's own default walk, so a second literal of the same
            // struct at other type arguments does not answer for it.
            let overlay = self.ann_ref(|facts| &facts.default_overlays, struct_lit.id);
            self.with_overlay(overlay, |this| {
                for (name, field_index, raw_ty, default) in &decl_fields {
                    if provided.contains(name) {
                        continue;
                    }
                    if let Some(default_expr) = default {
                        let expected_field_ty = substitute(this, *raw_ty);
                        // A default's free names resolve in the struct's module.
                        let value = ctx.with_caller_bindings_hidden(|ctx| {
                            this.with_module_perspective(&struct_module, |this| {
                                this.with_expr_ownership(default_expr, |this| {
                                    this.reify_expr(default_expr, ctx, Some(expected_field_ty))
                                })
                            })
                        });
                        fields.push(TirStructField {
                            name: name.clone(),
                            value,
                            field_index: *field_index,
                        });
                    }
                }
            });
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

        Self::hoist_block(literal, base_binding)
    }

    /// Bind each collected impure sub-piece to a `let $ca_N = …;` in `prelude`
    /// once and register its `AstId` → `Local` override, so every recurrence
    /// in the reified read / write becomes that `Local` read.
    fn bind_compound_hoists(
        &mut self,
        hoists: &[CompoundHoist<'_>],
        prelude: &mut Vec<TirStmt>,
        ctx: &mut FunctionContext,
    ) {
        for hoist in hoists {
            let value = self.reify_expr(hoist.piece, ctx, None);
            let name = minted_name("ca", ctx.fresh_serial());
            let local = bind_to_local(ctx, name, value, prelude);
            self.compound_overrides.insert(hoist.piece.id(), local);
        }
    }

    /// Wrap the compound-assign write in a `Block` that runs `prelude`
    /// (the once-bound sub-piece `let`s) first. With no bindings the flat
    /// write is returned unchanged, so pure targets (`x += 1`, `g += 1`)
    /// keep their previous WIR shape.
    fn wrap_prelude(&self, prelude: Vec<TirStmt>, write: TirExpr, span: Span) -> TirExpr {
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

    /// The combined value `read OP rhs`, through the operator's trait method
    /// where annotate dispatched one (`u128 /= u128` → `Div::div`).
    fn build_compound_combined(
        &mut self,
        read: TirExpr,
        rhs: TirExpr,
        op: TirBinaryOp,
        compound: &ast::CompoundAssignExpr,
    ) -> TirExpr {
        let combined_type = read.type_id;
        if let Some(dispatch) = self.ann_operator_dispatch(compound.id) {
            self.binary_operator_call(dispatch, read, rhs, compound.span)
        } else {
            TirExpr::new(
                TirExprKind::Binary {
                    left: Box::new(read),
                    op,
                    right: Box::new(rhs),
                },
                combined_type,
                compound.span,
            )
        }
    }

    /// The method the subscript `index` reads its element through.
    fn index_read_dispatch(&self, index: &ast::IndexExpr) -> Option<OperatorDispatch> {
        self.ann_operator_dispatch(index.id)
            .map(|dispatch| unchecked_subscript(index, dispatch))
    }

    /// The compound-assign read side of `recv[idx]`, reusing the same
    /// once-evaluated receiver / subscript the write side gets.
    fn build_index_trait_read(
        &self,
        recv: &TirExpr,
        idx: &TirExpr,
        index_expr: &ast::IndexExpr,
        span: Span,
        ctx: &mut FunctionContext,
    ) -> TirExpr {
        let Some(dispatch) = self.index_read_dispatch(index_expr) else {
            // Write-only `IndexAssign` type — annotate diagnosed the missing
            // read; match its recovery shape.
            return TirExpr::new(TirExprKind::Unit, TypeTable::ERROR, span);
        };
        // Deref result type: the index expr's recorded type, peeling
        // `&Output` on the degenerate missing-annotation path.
        let deref_type = self
            .tysys
            .index_deref_type(self.ann_expression_types(index_expr.id), &dispatch);
        let recv = self.adjust_receiver(
            recv.clone(),
            dispatch.self_kind,
            dispatch.is_ref_impl(),
            ctx,
        );
        self.tysys
            .build_index_read_from_dispatch(recv, idx.clone(), dispatch, deref_type, span)
    }

    /// `target = value`. A place's operands evaluate before the value.
    fn reify_write(
        &mut self,
        target: &ast::Expr,
        value: &ast::Expr,
        span: Span,
        ctx: &mut FunctionContext,
    ) -> TirExpr {
        let place = self.reify_write_place(target, ctx);
        let value = self.reify_expr(value, ctx, place.value_type());
        place.write(value, span)
    }

    /// The place `target` names. An `IndexAssign` target replays the
    /// dispatch `resolve_write_place` recorded on the subscript.
    fn reify_write_place(&mut self, target: &ast::Expr, ctx: &mut FunctionContext) -> WritePlace {
        if let ast::Expr::Index(index_expr) = target
            && let Some(dispatch) = self.ann_index_assign_dispatch(index_expr.id)
        {
            let receiver = self.reify_expr(&index_expr.expr, ctx, None);
            let receiver =
                self.adjust_receiver(receiver, dispatch.self_kind, dispatch.is_ref_impl(), ctx);
            let index = self.reify_expr(&index_expr.index, ctx, None);
            return WritePlace::IndexAssign {
                receiver,
                index: Box::new(index),
                dispatch: Box::new(dispatch),
            };
        }
        WritePlace::Place(self.reify_expr(target, ctx, None))
    }

    /// `[t0, t1, …] = value` as `{ let [$da_0, $da_1, …] = value; t0 = $da_0;
    /// t1 = $da_1; … }`: the whole value first, then each target in turn.
    /// The target places reify before the value, in the order annotate
    /// resolved them, so both walks allocate the same frame.
    fn reify_destructuring_assign(
        &mut self,
        assign: &ast::AssignExpr,
        targets: &[ast::Expr],
        ctx: &mut FunctionContext,
    ) -> TirExpr {
        let span = assign.span;
        if let Some(dispatch) = self.ann_index_swap_dispatch(assign.id) {
            let transposition = Transposition::of(assign).expect("annotate read a transposition");
            return self.reify_index_swap(&transposition, dispatch, span, ctx);
        }
        let places: Vec<WritePlace> = targets
            .iter()
            .map(|target| self.reify_write_place(target, ctx))
            .collect();
        let value = self.reify_expr(&assign.value, ctx, None);
        let element_types = self
            .tysys
            .type_table
            .borrow()
            .as_tuple(value.type_id)
            .expect("annotate checked the value is a tuple of one element per target");
        assert_eq!(element_types.len(), targets.len());
        let ctx = &mut ctx.enter_scope();
        let bindings: Vec<(String, u32, TypeId)> = element_types
            .into_iter()
            .map(|type_id| {
                let name = minted_name("da", ctx.fresh_serial());
                let index = ctx.add_local(name.clone(), type_id, false, None);
                (name, index, type_id)
            })
            .collect();
        let pattern = TirPattern::Tuple(
            bindings
                .iter()
                .map(|(name, local_index, type_id)| TirPattern::Binding {
                    name: name.clone(),
                    local_index: *local_index,
                    type_id: *type_id,
                })
                .collect(),
            false,
        );
        let mut stmts = vec![TirStmt::new(
            TirStmtKind::LetDestructure { pattern, value },
            span,
        )];
        for (place, (name, index, type_id)) in places.into_iter().zip(bindings) {
            let element = self.handed_on(TirExpr::new(
                TirExprKind::Local { index, name },
                type_id,
                span,
            ));
            stmts.push(TirStmt::new(
                TirStmtKind::Expr(place.write(element, span)),
                span,
            ));
        }
        TirExpr::new(
            TirExprKind::Block(TirBlock::new(stmts, span)),
            TypeTable::UNIT,
            span,
        )
    }

    /// A receiver spelled as the first argument (`Trait::method(recv)`) given
    /// the references its `self` declares.
    fn adjust_receiver_in_args(
        &self,
        args: &mut [CallArg],
        dispatch: &StaticMethodDispatch,
        span: Span,
    ) {
        if !dispatch.self_in_args {
            return;
        }
        let (Some(receiver), Some(&self_type)) = (args.first_mut(), dispatch.param_types.first())
        else {
            return;
        };
        let spelled = std::mem::replace(
            &mut receiver.expr,
            TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, span),
        );
        receiver.expr = to_layers_of(spelled, self_type, &self.tysys.type_table.borrow());
    }

    /// [`adjust_receiver_for_self_kind`] for a dispatched receiver. A
    /// `&mut self` receiver the callee can replace rather than write into is
    /// boxed, or the mutation has no slot to land in and mutates a copy.
    fn adjust_receiver(
        &self,
        receiver: TirExpr,
        self_kind: ast::SelfKind,
        is_ref_impl: bool,
        ctx: &mut FunctionContext,
    ) -> TirExpr {
        if !is_ref_impl
            && matches!(self_kind, ast::SelfKind::MutRef)
            && self.tysys.mut_self_receiver_needs_box(receiver.type_id)
            && let TirExprKind::Local { index, .. } = &receiver.kind
        {
            ctx.address_taken_locals.insert(*index);
        }
        adjust_receiver_for_self_kind(receiver, self_kind, is_ref_impl, &self.tysys.type_table)
    }

    /// `[c[i], c[j]] = [c[j], c[i]]` as `{ let $da_N = j; c.index_swap(i, $da_N) }`,
    /// which evaluates `j` and `i` in the order the desugaring first reads
    /// them. `c` is a path neither can change.
    fn reify_index_swap(
        &mut self,
        transposition: &Transposition<'_>,
        dispatch: OperatorDispatch,
        span: Span,
        ctx: &mut FunctionContext,
    ) -> TirExpr {
        let receiver = self.reify_expr(transposition.container, ctx, None);
        let receiver =
            self.adjust_receiver(receiver, dispatch.self_kind, dispatch.is_ref_impl(), ctx);
        let j = self.reify_expr(transposition.j, ctx, None);
        let i = self.reify_expr(transposition.i, ctx, None);
        let ctx = &mut ctx.enter_scope();
        let name = minted_name("da", ctx.fresh_serial());
        let type_id = j.type_id;
        let local_index = ctx.add_local(name.clone(), type_id, false, None);
        let bind_j = TirStmt::new(
            TirStmtKind::Let {
                name: name.clone(),
                local_index,
                is_mut: false,
                type_id,
                value: j,
                storage: LetStorage::Planned,
            },
            span,
        );
        let j = self.handed_on(TirExpr::new(
            TirExprKind::Local {
                index: local_index,
                name,
            },
            type_id,
            span,
        ));
        let mut function_ref = dispatch.function_ref;
        if transposition.vouched {
            function_ref.retarget_to_unchecked_twin();
        }
        let swap = build_tir_method_call(
            receiver,
            function_ref,
            vec![],
            vec![CallArg::new(i, false), CallArg::new(j, false)],
            dispatch.return_type,
            span,
        );
        TirExpr::new(
            TirExprKind::Block(TirBlock::new(
                vec![bind_j, TirStmt::new(TirStmtKind::Expr(swap), span)],
                span,
            )),
            TypeTable::UNIT,
            span,
        )
    }

    /// Reify a compound assignment `x += y` as `x = x op y`, evaluating each
    /// target sub-expression once: impure value operands bind to `let $ca_N`
    /// while the place skeleton stays inline. Residual: a nested index's
    /// receiver (`m.index(i)` in `m[i][j] += 1`) is still duplicated, so a
    /// side-effecting custom `Index::index` runs twice.
    fn reify_compound_assign(
        &mut self,
        compound: &ast::CompoundAssignExpr,
        ctx: &mut FunctionContext,
    ) -> TirExpr {
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

        // A nested compound assign keeps its own overrides; the scope keeps
        // `$ca_N` names out of the enclosing map.
        let mut hoists: Vec<CompoundHoist<'_>> = Vec::new();
        collect_compound_hoists(&compound.target, &mut hoists);
        util::replaced(
            self,
            |reify| &mut reify.compound_overrides,
            IndexMap::default(),
            |this| {
                let ctx = &mut ctx.enter_scope();
                let mut prelude: Vec<TirStmt> = Vec::new();
                this.bind_compound_hoists(&hoists, &mut prelude, ctx);
                this.reify_compound_assign_body(compound, ctx, op, prelude, span)
            },
        )
        .0
    }

    /// Build the read + write of a compound assign with the impure-operand
    /// overrides active.
    fn reify_compound_assign_body(
        &mut self,
        compound: &ast::CompoundAssignExpr,
        ctx: &mut FunctionContext,
        op: TirBinaryOp,
        prelude: Vec<TirStmt>,
        span: Span,
    ) -> TirExpr {
        // `recv[idx] OP= v` on an `IndexAssign` type: the read
        // (`*recv.index(idx)`) and the write (`recv.index_assign(idx, …)`) are
        // different methods built from the same reified receiver / subscript.
        if let ast::Expr::Index(index_expr) = &compound.target
            && let Some(dispatch) = self.ann_index_assign_dispatch(index_expr.id)
        {
            let recv = self.reify_expr(&index_expr.expr, ctx, None);
            let index = self.reify_expr(&index_expr.index, ctx, None);

            let read = self.build_index_trait_read(&recv, &index, index_expr, span, ctx);
            let rhs = self.reify_expr(&compound.value, ctx, Some(read.type_id));
            let combined = self.build_compound_combined(read, rhs, op, compound);

            let receiver =
                self.adjust_receiver(recv, dispatch.self_kind, dispatch.is_ref_impl(), ctx);
            let place = WritePlace::IndexAssign {
                receiver,
                index: Box::new(index),
                dispatch: Box::new(dispatch),
            };
            return self.wrap_prelude(prelude, place.write(combined, span), span);
        }

        // General l-values (local, global, field-access, tuple index, deref):
        // the reified place's impure operands are already overridden to bound
        // locals, so read (a clone) and write share a pure skeleton.
        let place = self.reify_expr(&compound.target, ctx, None);
        let rhs = self.reify_expr(&compound.value, ctx, Some(place.type_id));
        let combined = self.build_compound_combined(place.clone(), rhs, op, compound);
        let write = WritePlace::Place(place).write(combined, span);
        self.wrap_prelude(prelude, write, span)
    }

    /// Reify the `?` postfix operator into the `Match` its operand type calls
    /// for: `match expr { Some(v) => v, None => return null }` for `Option<T>`,
    /// `match expr { Ok(v) => v, Err(e) => return Err(From::from(e)) }` for
    /// `Result<T, E>`. Annotate has already validated both the operand and the
    /// function's return type.
    fn reify_question_mark(&mut self, qm: &ast::TryOpExpr, ctx: &mut FunctionContext) -> TirExpr {
        let inner = self.reify_expr(&qm.expr, ctx, None);
        let inner_type = inner.type_id;

        let (is_option, is_result) = {
            let tt = self.tysys.type_table.borrow();
            (
                tt.as_option(inner_type).is_some(),
                matches!(
                    tt.get(inner_type),
                    ResolvedType::GenericInstance { .. } if tt.is_result(inner_type)
                ),
            )
        };

        let mut desugared = if is_option {
            self.reify_question_mark_option(inner, ctx, qm.span)
        } else if is_result {
            self.reify_question_mark_result(inner, ctx, qm.span, qm.id)
        } else {
            // Annotate already diagnosed; produce a Unit-typed
            // placeholder of `ERROR` so downstream phases see the
            // same shape annotate's recovery path produced.
            return TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, qm.span);
        };
        // The early return is the desugared match's last arm.
        if let TirExprKind::Match { arms, .. } = &mut desugared.kind
            && let Some(exit) = arms.last_mut()
        {
            let placeholder = TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, qm.span);
            let body = std::mem::replace(&mut exit.body, placeholder);
            exit.body = self.probe_expr(ProbeSite::TryExit, qm.id, body);
        }
        desugared
    }

    /// `Option<T>`'s `?`-op desugar — mirrors
    /// `Elaborator::resolve_question_mark_option`.
    fn reify_question_mark_option(
        &mut self,
        inner: TirExpr,
        ctx: &mut FunctionContext,
        span: Span,
    ) -> TirExpr {
        let inner_type = inner.type_id;
        let (some_type, some_name, some_index, none_name, none_index) = {
            let tt = self.tysys.type_table.borrow();
            let some_type = tt.as_option(inner_type).unwrap();
            let items = tt.compiler_items();
            let (_, _, some_n, some_i) = items.require_variant_case(CompilerItem::OptionSome);
            let (_, _, none_n, none_i) = items.require_variant_case(CompilerItem::OptionNone);
            (
                some_type,
                some_n.to_string(),
                some_i,
                none_n.to_string(),
                none_i,
            )
        };

        let v_local = ctx
            .enter_scope()
            .add_local("$qm_v".to_string(), some_type, false, None);

        let some_arm = TirMatchArm {
            pattern: TirPattern::Variant {
                enum_type: inner_type,
                variant_name: some_name,
                case_index: some_index,
                bindings: vec![TirPattern::Binding {
                    name: "$qm_v".to_string(),
                    local_index: v_local,
                    type_id: some_type,
                }],
                payload_type: some_type,
            },
            guard: None,
            body: TirExpr::new(
                TirExprKind::Local {
                    index: v_local,
                    name: "$qm_v".to_string(),
                },
                some_type,
                span,
            )
            .moved(),
            span,
            consumes: true,
        };

        let none_arm = TirMatchArm {
            pattern: TirPattern::Variant {
                enum_type: inner_type,
                variant_name: none_name,
                case_index: none_index,
                bindings: vec![],
                payload_type: TypeTable::UNIT,
            },
            guard: None,
            body: TirExpr::new(
                TirExprKind::Block(TirBlock::new(
                    vec![
                        // A failure is no trap, so its rarity is a guess.
                        cold_path_stmt(span),
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
            consumes: true,
        };

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
        span: Span,
        qm_id: AstId,
    ) -> TirExpr {
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

        let mut scope = ctx.enter_scope();
        let v_local = scope.add_local("$qm_v".to_string(), ok_type, false, None);
        let e_local = scope.add_local("$qm_e".to_string(), inner_err_type, false, None);

        let (ok_name, ok_index, err_name, err_index) = {
            let tt = self.tysys.type_table.borrow();
            let items = tt.compiler_items();
            let (_, _, ok_n, ok_i) = items.require_variant_case(CompilerItem::ResultOk);
            let (_, _, err_n, err_i) = items.require_variant_case(CompilerItem::ResultErr);
            (ok_n.to_string(), ok_i, err_n.to_string(), err_i)
        };

        let ok_arm = TirMatchArm {
            pattern: TirPattern::Variant {
                enum_type: inner_type,
                variant_name: ok_name,
                case_index: ok_index,
                bindings: vec![TirPattern::Binding {
                    name: "$qm_v".to_string(),
                    local_index: v_local,
                    type_id: ok_type,
                }],
                payload_type: ok_type,
            },
            guard: None,
            body: TirExpr::new(
                TirExprKind::Local {
                    index: v_local,
                    name: "$qm_v".to_string(),
                },
                ok_type,
                span,
            )
            .moved(),
            span,
            consumes: true,
        };

        let e_expr = TirExpr::new(
            TirExprKind::Local {
                index: e_local,
                name: "$qm_e".to_string(),
            },
            inner_err_type,
            span,
        )
        .moved();
        // `<OuterErr>::from(e)` where an impl converts, or a bound promises
        // one; otherwise the two convert with no impl, and `e` is cast.
        let converted_err = match self.ann_from_call_facts(qm_id) {
            Some(facts) => self.reify_from_call(outer_err_type, e_expr, span, facts),
            None => bare_cast(e_expr, outer_err_type, span),
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
                case_index: err_index,
                bindings: vec![TirPattern::Binding {
                    name: "$qm_e".to_string(),
                    local_index: e_local,
                    type_id: inner_err_type,
                }],
                payload_type: inner_err_type,
            },
            guard: None,
            body: TirExpr::new(
                TirExprKind::Block(TirBlock::new(
                    vec![
                        cold_path_stmt(span),
                        TirStmt::new(
                            TirStmtKind::Return {
                                value: Some(err_variant),
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
            consumes: true,
        };

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
    /// `let $m_0 = a; let $m_1 = b; ($m_0 < $m_1) & ($m_1 < c) …`.
    fn reify_comparison_chain(
        &mut self,
        chain: &ast::ComparisonChainExpr,
        ctx: &mut FunctionContext,
    ) -> TirExpr {
        assert!(
            chain.comparisons.len() >= 2,
            "the parser makes a single comparison a `Binary`"
        );
        let ctx = &mut ctx.enter_scope();
        let mut stmts: Vec<TirStmt> = Vec::new();

        let cmp0 = &chain.comparisons[0];
        let first_tir = self.reify_expr(&chain.first, ctx, None);
        let right0_tir = self.reify_expr(&cmp0.right, ctx, Some(first_tir.type_id));
        let first_ref = Self::bind_chain_operand(first_tir, &mut stmts, ctx);
        let right0_ref = Self::bind_chain_operand(right0_tir, &mut stmts, ctx);

        let mut acc_tir = self.reify_binary_op(
            cmp0.id,
            cmp0.op,
            first_ref,
            right0_ref.clone(),
            TypeTable::BOOL,
            cmp0.op_span,
        );
        let mut prev_tir = right0_ref;

        let last_idx = chain.comparisons.len() - 1;
        for idx in 1..chain.comparisons.len() {
            let cmp = &chain.comparisons[idx];
            let raw_right = self.reify_expr(&cmp.right, ctx, Some(prev_tir.type_id));
            let right_tir = if idx == last_idx {
                raw_right
            } else {
                Self::bind_chain_operand(raw_right, &mut stmts, ctx)
            };
            let next_prev = right_tir.clone();
            let cmp_tir = self.reify_binary_op(
                cmp.id,
                cmp.op,
                prev_tir,
                right_tir,
                TypeTable::BOOL,
                cmp.op_span,
            );
            acc_tir = TirExpr::new(
                TirExprKind::Binary {
                    left: Box::new(acc_tir),
                    op: TirBinaryOp::BitAnd,
                    right: Box::new(cmp_tir),
                },
                TypeTable::BOOL,
                chain.span,
            );
            prev_tir = next_prev;
        }

        stmts.push(TirStmt::new(TirStmtKind::Expr(acc_tir), chain.span));
        TirExpr::new(
            TirExprKind::Block(TirBlock::new(stmts, chain.span)),
            TypeTable::BOOL,
            chain.span,
        )
    }

    /// `let $m_K = value;` pushed onto `stmts`, answered by a read of `$m_K`.
    fn bind_chain_operand(
        value: TirExpr,
        stmts: &mut Vec<TirStmt>,
        ctx: &mut FunctionContext,
    ) -> TirExpr {
        let name = minted_name("m", ctx.fresh_serial());
        bind_to_local(ctx, name, value, stmts)
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
        let receiver = self.reify_expr(&index.expr, ctx, None);
        if self.tysys.type_table.borrow().is_never(receiver.type_id) {
            // The key never runs, yet resolve walked it, so its probes and
            // slots are reified here, after the receiver that diverges.
            let (type_id, span) = (receiver.type_id, receiver.span);
            let key = self.reify_expr(&index.index, ctx, None);
            let stmts = vec![
                TirStmt::new(TirStmtKind::Expr(receiver), span),
                TirStmt::new(TirStmtKind::Expr(key), span),
            ];
            return TirExpr::new(
                TirExprKind::Block(TirBlock::new(stmts, span)),
                type_id,
                span,
            );
        }

        // Tuple constant-index path: detect via the receiver's
        // resolved type + the index being a constant integer
        // literal. Matches `Elaborator::resolve_index`'s tuple
        // branch.
        let tuple_elems: Option<Vec<TypeId>> = {
            let base = receiver.type_id;
            let unwrapped = self.tysys.through_ref(base);
            self.tysys.type_table.borrow().as_tuple(unwrapped)
        };
        if let Some(elems) = &tuple_elems
            && let ast::Expr::Literal(lit) = &index.index
            && let ast::Literal::Number(repr) = &lit.value
            && let Some(digits) = util::integer_digits(repr)
            && let Ok(idx) = digits.parse::<usize>()
            && let Ok(elem) = self.tysys.tuple_literal_index_type(elems, idx)
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
            && let Some(walk) = ctx.variadic_enumerate_walk(&index.index)
            && let Some(elem_type) = self.tysys.pack_element_type(elems, walk)
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
        if let Some(dispatch) = self.index_read_dispatch(index) {
            let idx_expr = self.reify_expr(&index.index, ctx, None);
            let deref_type = self.tysys.index_deref_type(Some(recorded_type), &dispatch);
            let receiver =
                self.adjust_receiver(receiver, dispatch.self_kind, dispatch.is_ref_impl(), ctx);
            return self.tysys.build_index_read_from_dispatch(
                receiver, idx_expr, dispatch, deref_type, index.span,
            );
        }

        // No dispatch recorded → the elaborator emitted a recovery
        // shape (annotate would have diagnosed missing trait impl).
        // Match the recovery output with a Unit placeholder typed
        // as ERROR.
        TirExpr::new(TirExprKind::Unit, TypeTable::ERROR, index.span)
    }

    /// Reify a closure expression from the capture analysis in
    /// `sem.types.closure_captures[closure.id]`: `mut_captures` materialise as
    /// `let $ref_v = &mut v;` ahead of the body in declaration order, `captures`
    /// is the final capture list, and `is_mutating` picks `fn mut(…)`. Follows
    /// `resolve_closure` step by step.
    fn reify_closure(
        &mut self,
        closure: &ast::ClosureExpr,
        ctx: &mut FunctionContext,
        expected_type: Option<TypeId>,
    ) -> TirExpr {
        let span = closure.span;

        let cap_info = self
            .ann_closure_captures(closure.id)
            .expect("resolve_closure records the capture info for every closure reify emits");

        // Step 1 (replay): materialise outer-scope `$ref_v` locals
        // for each mut-capture in the recorded order; emit the
        // matching `let $ref_v = &mut v;` TIR; register
        // `deref_overrides` so the closure body's references to
        // captured mut-locals dereference the proxy.
        let mut ref_stmts: Vec<TirStmt> = Vec::new();
        let mut deref_overrides: hashmap::IndexMap<String, (String, TypeId)> =
            hashmap::IndexMap::default();
        for mc in &cap_info.mut_captures {
            let Some(outer) = ctx.lookup(&mc.var_name) else {
                unreachable!(
                    "in {}: annotate mut-captured `{}`, which this frame does not bind",
                    ctx.function_name, mc.var_name
                )
            };
            let outer_index = outer.index;
            let ref_index = ctx.add_local(mc.ref_name.clone(), mc.ref_type, false, None);
            ctx.address_taken_locals.insert(outer_index);
            ctx.mut_captured_locals.insert(outer_index);
            ref_stmts.push(TirStmt::new(
                TirStmtKind::Let {
                    name: mc.ref_name.clone(),
                    local_index: ref_index,
                    is_mut: false,
                    type_id: mc.ref_type,
                    value: TirExpr::new(
                        TirExprKind::Unary {
                            op: TirUnaryOp::MutRef,
                            expr: Box::new(TirExpr::new(
                                TirExprKind::Local {
                                    index: outer_index,
                                    name: mc.var_name.clone(),
                                },
                                mc.inner_type,
                                span,
                            )),
                        },
                        mc.ref_type,
                        span,
                    ),
                    storage: LetStorage::Planned,
                },
                span,
            ));
            deref_overrides.insert(mc.var_name.clone(), (mc.ref_name.clone(), mc.inner_type));
        }

        // Step 2: open the closure context with the deref overrides and the
        // environment annotate settled on, whose slots the body walk reads.
        let mut closure_ctx =
            FunctionContext::new_closure(TypeTable::UNKNOWN, ctx, &self.tysys.type_table);
        closure_ctx.deref_overrides = deref_overrides;
        closure_ctx.seed_captures(cap_info.captures.iter().map(|c| c.name.as_str()));

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
        let block_return_type = if let Expr::Block(ref block) = closure.body {
            let ctrl_ctx = CtrlFlowCtx {
                expression_types: &self.types.expression_types,
                type_table: &self.tysys.type_table,
            };
            find_return_type_in_block(ctrl_ctx, block)
        } else {
            None
        };
        if let Some(rt) = body_expected.or(block_return_type) {
            closure_ctx.return_type = rt;
        }

        let body = self.reify_expr(&closure.body, &mut closure_ctx, body_expected);

        // Step 5: assemble the capture list, making this frame capture whatever
        // the closure only reached through it, as `resolve_closure` also did.
        let captures = relink_recorded_captures(
            &cap_info.captures,
            &closure_ctx,
            ctx,
            &self.tysys.type_table.borrow(),
        );

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
        let body = self.probe_expr(ProbeSite::Around, closure.body.id(), body);

        let param_types: Vec<TypeId> = params.iter().map(|(_, t)| *t).collect();
        let func_type = self.tysys.type_table.borrow_mut().make_function_with_mut(
            cap_info.is_mutating,
            param_types,
            return_type,
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
            Some(cap_info.declared_effects.clone()).filter(|effects| !effects.is_empty());

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
        // outer-scope `$ref_v` bindings.
        if ref_stmts.is_empty() {
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

    /// Record on a default's `TypePackExpansion` the tuple the call settled the
    /// pack to, read off the parameter's own concrete type. The caller it is
    /// spliced into may be one nothing instantiates, so this is the last chance.
    fn settle_packs_in_default(&mut self, expr: &mut TirExpr, expected: TypeId) {
        let TirExprKind::TupleLiteral { elements } = &mut expr.kind else {
            return;
        };
        // One expansion and no spread: every other element is one tuple slot,
        // so the expansion covers exactly the slots between the two runs.
        let is_expansion = |e: &TirExpr| matches!(e.kind, TirExprKind::TypePackExpansion { .. });
        let Some(at) = elements.iter().position(is_expansion) else {
            return;
        };
        if elements.iter().skip(at + 1).any(is_expansion)
            || elements
                .iter()
                .any(|e| matches!(e.kind, TirExprKind::TupleSpread { .. }))
        {
            return;
        }
        let Some(type_args) = self.tysys.type_table.borrow().as_tuple(expected) else {
            return;
        };
        let after = elements.len() - at - 1;
        if type_args.len() < at + after {
            return;
        }
        let settled: Vec<TypeId> = type_args[at..type_args.len() - after].to_vec();
        if settled
            .iter()
            .any(|&t| self.tysys.type_table.borrow().contains_type_pack(t))
        {
            return;
        }
        let settled = self.tysys.type_table.borrow_mut().make_tuple(settled);
        let TirExprKind::TypePackExpansion { settled_pack, .. } = &mut elements[at].kind else {
            unreachable!("the element at `at` is what `is_expansion` matched")
        };
        *settled_pack = Some(settled);
    }

    /// Reify a tuple literal, spreads included. Its type is built bottom-up so a nested
    /// tuple's element type is the inner literal's interned id, which `nir/sroa` relies on.
    fn reify_tuple_literal(
        &mut self,
        tuple_lit: &ast::TupleLiteralExpr,
        ctx: &mut FunctionContext,
        recorded_type: TypeId,
        span: Span,
    ) -> TirExpr {
        let mut elements: Vec<TirExpr> = Vec::new();
        let mut elem_types: Vec<TypeId> = Vec::new();
        // (local_idx, name, expr, span) for non-trivial spread operands.
        let mut spread_bindings: Vec<(u32, String, TirExpr, Span)> = Vec::new();

        for elem in &tuple_lit.elements {
            if let ast::Expr::Spread(inner, _span) = elem {
                let spread_expr = self.reify_expr(inner, ctx, None);
                let contains_pack = self
                    .tysys
                    .type_table
                    .borrow()
                    .contains_type_pack(spread_expr.type_id);
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
                                settled_pack: None,
                            },
                            *elem_types.last().unwrap(),
                            elem.span(),
                        ));
                    } else {
                        let spread_type_id = spread_expr.type_id;
                        elem_types.extend(
                            self.tysys
                                .type_table
                                .borrow()
                                .elem_types_or_self(spread_type_id),
                        );
                        elements.push(TirExpr::new(
                            TirExprKind::TupleSpread {
                                expr: Box::new(spread_expr),
                            },
                            spread_type_id,
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
                            settled_pack: None,
                        },
                        mapped,
                        elem.span(),
                    ));
                } else if let ResolvedType::GenericInstance {
                    def,
                    type_args: inner_elems,
                } = spread_type
                    && self.tysys.type_table.borrow().is_tuple_def(def)
                {
                    // Concrete tuple: expand inline via FieldAccess. Bind a
                    // non-trivial operand to a temporary for single evaluation.
                    let spread_ref = if matches!(spread_expr.kind, TirExprKind::Local { .. }) {
                        spread_expr
                    } else {
                        let spread_type_id = spread_expr.type_id;
                        let tmp_name = minted_name("spread", ctx.fresh_serial());
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
                        elements.push(self.handed_on(TirExpr::new(
                            TirExprKind::FieldAccess {
                                expr: Box::new(spread_ref.clone()),
                                field_index: i as u32,
                                field_name: i.to_string(),
                            },
                            et,
                            elem.span(),
                        )));
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
                // A diverging element takes the type the tuple is expected to hold
                // there, and stays `!` where nothing expects one.
                let table = self.tysys.type_table.borrow();
                let elem_type = match table.as_tuple(recorded_type) {
                    Some(expected) if table.is_never(resolved.type_id) => {
                        expected[elem_types.len()]
                    }
                    _ => resolved.type_id,
                };
                drop(table);
                elem_types.push(elem_type);
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
                            type_id,
                            storage: LetStorage::Planned,
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
        facts: SequenceCoercionFacts,
        ctx: &mut FunctionContext,
        span: Span,
    ) -> TirExpr {
        let elements = tuple_lit
            .elements
            .iter()
            .map(|element| self.reify_literal_element(element, facts.element_type, ctx))
            .collect();
        let array = TirExpr::new(
            TirExprKind::ArrayLiteral { elements },
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
        facts: KeyValueCoercionFacts,
        ctx: &mut FunctionContext,
        span: Span,
    ) -> TirExpr {
        let string_type = self
            .tysys
            .type_table
            .borrow_mut()
            .make_compiler_struct(CompilerItem::String);

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
        let label = "$kv_lit".to_string();
        let ctx = &mut ctx.enter_scope();
        let mut stmts: Vec<TirStmt> = Vec::new();
        let mut acc: Option<u32> = None;
        let mut run: Vec<TirExpr> = Vec::new();
        let mut members: Vec<(TirExpr, Span)> = Vec::new();
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
                    let index = ctx.add_local("$acc".to_string(), output_type, true, None);
                    stmts.push(TirStmt::new(
                        TirStmtKind::Let {
                            name: "$acc".to_string(),
                            local_index: index,
                            is_mut: true,
                            type_id: output_type,
                            value,
                            storage: LetStorage::Planned,
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
                        name: "$acc".to_string(),
                    },
                    output_type,
                    span,
                )),
            },
            span,
        ));

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
        slot_type: tir::TypeId,
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
        facts: &KeyValueCoercionFacts,
        string_type: tir::TypeId,
        ctx: &mut FunctionContext,
    ) -> TirExpr {
        let key = TirExpr::new(
            TirExprKind::StringLiteral(field.name.clone()),
            string_type,
            field.name_span,
        );
        let value = self.reify_literal_element(&field.value, facts.value_type, ctx);
        TirExpr::new(
            TirExprKind::TupleLiteral {
                elements: vec![key, value],
            },
            facts.pair_type,
            field.span,
        )
    }

    /// The `From::from(value)` call a `?` converts its error through, from
    /// the facts [`super::Elaborator::resolve_from_call`] recorded.
    fn reify_from_call(
        &mut self,
        target_type: TypeId,
        value: TirExpr,
        span: Span,
        facts: FromCallFacts,
    ) -> TirExpr {
        let generic = LocalMethodName::new(
            facts.target_name,
            Some(facts.trait_name),
            "from".to_string(),
        );
        let arg_names: Vec<FqTypeName> = {
            let table = self.tysys.type_table.borrow();
            facts
                .impl_type_args
                .iter()
                .map(|&arg| table.fq_type_name_rowless(arg))
                .collect()
        };
        let mut method_info = generic.with_type_args(&arg_names, &[]);
        method_info.is_type_param_receiver = facts.type_param_receiver;
        let monomorph_info = (!facts.impl_type_args.is_empty()).then(|| MonomorphInfo {
            generic_name: generic.to_mangled_name(),
            impl_type_args: facts.impl_type_args,
            method_type_args: vec![],
            is_blanket: false,
        });

        TirExpr::new(
            TirExprKind::Call {
                func: Box::new(FunctionRef {
                    // A parameter receiver: the instance's dispatch picks.
                    template: (!facts.type_param_receiver).then(|| {
                        let def = facts
                            .method_def
                            .expect("a selected `From` block declares `from`");
                        self.tysys.signatures.declared_template(def)
                    }),
                    module_source: facts.module_source,
                    name: method_info.to_mangled_name(),
                    monomorph_info,
                    method_info: Some(method_info),
                }),
                type_args: vec![],
                args: CallArgs::free(vec![CallArg::new(value, false)]),
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
        result_type: tir::TypeId,
    ) -> TirExpr {
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
                    effect: entry.effect.clone(),
                    trait_type_args: entry.trait_type_args.clone(),
                    handler: handler.clone(),
                    handler_type: facts.handler_type,
                    span: binding.span,
                    bundle_group: facts.bundle_group,
                });
            }
        }

        // `with ... do { ... }` evaluates to its body block's trailing value.
        // Propagate the recorded result type so the body's tail expression
        // replays any coercion (e.g. literal widening) the annotate phase
        // applied against the binding's expected type.
        let body = self.reify_block(&with_expr.body, &mut ctx.enter_scope(), Some(result_type));

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
        let scrutinee = self.reify_expr(&m.expr, ctx, None);
        let scrutinee_type = scrutinee.type_id;

        let ctx = &mut ctx.enter_scope();
        let pattern_tir = self.reify_pattern(&m.pattern, scrutinee_type, m.span, ctx);
        let arm_body = match &m.guard {
            Some(guard) => self.reify_condition_expr(guard, ctx),
            None => bool_lit(true, m.span),
        };

        let arms = vec![
            TirMatchArm {
                pattern: pattern_tir,
                guard: None,
                body: arm_body,
                span: m.span,
                consumes: false,
            },
            TirMatchArm {
                pattern: TirPattern::Wildcard,
                guard: None,
                body: bool_lit(false, m.span),
                span: m.span,
                consumes: false,
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
        let Some((head, type_args)) = peel_to_struct(&self.tysys.type_table.borrow(), type_id)
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
        let subst: hashmap::IndexMap<u32, TypeId> = (0..type_args.len() as u32)
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

    /// Reify an anonymous struct literal `{ x: 1, y: 2 }` as the struct annotate
    /// registered under the name its field types derive.
    fn reify_anonymous_struct_literal(
        &mut self,
        struct_lit: &ast::StructLiteralExpr,
        ctx: &mut FunctionContext,
        recorded_type: TypeId,
    ) -> TirExpr {
        // `resolve_anonymous_struct_literal` records the synthesised `$anon_{…}`
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
        let mut base_types: Vec<TypeId> = vec![TypeTable::UNKNOWN; struct_lit.spreads.len()];
        let mut explicit_refs: Vec<Option<TirExpr>> = vec![None; struct_lit.fields.len()];
        let mut explicit_types: Vec<TypeId> = vec![TypeTable::UNKNOWN; struct_lit.fields.len()];
        for member in struct_lit.members() {
            match member {
                ast::LiteralMember::Spread(si, sp) => {
                    let expr = self.reify_expr(&sp.expr, ctx, None);
                    base_types[si] = expr.type_id;
                    base_refs[si] = Some(Self::hoist_once(ctx, expr, "base", &mut stmts));
                }
                ast::LiteralMember::Field(pos, f) => {
                    let expr = self.reify_expr(&f.value, ctx, None);
                    explicit_types[pos] = expr.type_id;
                    explicit_refs[pos] = Some(Self::hoist_once(ctx, expr, "fld", &mut stmts));
                }
            }
        }

        let base_field_lists: Vec<Vec<(String, TypeId, u32)>> = base_types
            .iter()
            .map(|&t| self.spread_base_field_list(t))
            .collect();
        let plan = compose_union_plan(struct_lit, &base_field_lists, &explicit_types);
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
                let value = self.handed_on(value);
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

        Self::hoist_block(literal, stmts)
    }

    /// Bind `expr` to a fresh temporary [`minted_name`] names from `what`
    /// (pushed onto `stmts`) so it evaluates once in place, unless it is already
    /// a local. Returns a reference.
    fn hoist_once(
        ctx: &mut FunctionContext,
        expr: TirExpr,
        what: &str,
        stmts: &mut Vec<TirStmt>,
    ) -> TirExpr {
        if matches!(expr.kind, TirExprKind::Local { .. }) {
            return expr;
        }
        let name = minted_name(what, ctx.fresh_serial());
        bind_to_local(ctx, name, expr, stmts)
    }

    /// [`Self::hoist_once`] for a receiver, so it runs ahead of the arguments. A
    /// place stays in its slot for `&mut self` to write; only its subscripts hoist.
    fn bind_receiver_ahead(
        &self,
        ctx: &mut FunctionContext,
        receiver: TirExpr,
        stmts: &mut Vec<TirStmt>,
    ) -> TirExpr {
        if !self.tysys.is_source_place(&receiver) {
            return Self::hoist_once(ctx, receiver, "recv", stmts);
        }
        self.bind_subscripts_ahead(ctx, receiver, "recv_index", stmts)
    }

    /// `place` with each subscript [`Self::hoist_once`]d in order, so the place
    /// itself stays where it is written.
    fn bind_subscripts_ahead(
        &self,
        ctx: &mut FunctionContext,
        mut place: TirExpr,
        what: &str,
        stmts: &mut Vec<TirStmt>,
    ) -> TirExpr {
        let type_table = self.tysys.type_table.borrow();
        for subscript in source_place_subscripts_mut(&mut place, type_table.compiler_items()) {
            let unbound = TirExpr::new(TirExprKind::Unit, TypeTable::UNIT, subscript.span);
            let value = std::mem::replace(subscript, unbound);
            *subscript = Self::hoist_once(ctx, value, what, stmts);
        }
        place
    }

    /// `value` inside the block holding the [`Self::hoist_once`] temporaries it
    /// reads, or `value` alone when nothing was hoisted.
    fn hoist_block(value: TirExpr, mut stmts: Vec<TirStmt>) -> TirExpr {
        if stmts.is_empty() {
            return value;
        }
        let (span, type_id) = (value.span, value.type_id);
        stmts.push(TirStmt::new(TirStmtKind::Expr(value), span));
        TirExpr::new(
            TirExprKind::Block(TirBlock::new(stmts, span)),
            type_id,
            span,
        )
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
                let ctx = &mut ctx.enter_scope();
                let pattern = self.reify_pattern(&arm.pattern, scrutinee_type, arm.span, ctx);
                let guard = arm
                    .guard
                    .as_ref()
                    .map(|g| self.reify_condition_expr(g, ctx));
                let body = self.reify_expr(&arm.body, ctx, branch_expected);
                let body = self.probe_expr(ProbeSite::Around, arm.body.id(), body);
                TirMatchArm {
                    pattern,
                    guard,
                    body,
                    span: arm.span,
                    consumes: true,
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

    /// `Type::from(x)` with no impl behind it, as annotate recorded it at
    /// `call`: a reflexive or newtype conversion.
    fn reify_conversion_without_impl(
        &mut self,
        call: AstId,
        args: &[ast::Expr],
        ctx: &mut FunctionContext,
        recorded_type: TypeId,
        span: Span,
    ) -> Option<TirExpr> {
        let cast = match self.ann_desugars(call)? {
            DesugarKind::NewtypeFromCollapse => false,
            DesugarKind::NewtypeFromUnwrap | DesugarKind::NewtypeFromWrap => true,
            _ => return None,
        };
        let arg = self.reify_expr(&args[0], ctx, None);
        Some(if cast {
            bare_cast(arg, recorded_type, span)
        } else {
            arg
        })
    }

    /// Reify a `StaticMethodCallExpr` — a fully-qualified static call like
    /// `Stream<u8>::new()`, whose target parses as a `Type` node rather than an
    /// `Ident` callee. Same shape as `reify_call`'s qualified-callee branch:
    /// resolve the target type, take the impl module from the resolved struct,
    /// and build the mangled `$Type__method` `FunctionRef`.
    fn reify_static_method_call(
        &mut self,
        static_call: &ast::StaticMethodCallExpr,
        ctx: &mut FunctionContext,
        recorded_type: TypeId,
    ) -> TirExpr {
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
            self.adjust_receiver_in_args(&mut args, &dispatch, static_call.span);

            let prelude = self.reify_apply_param_defaults(
                &mut args,
                &dispatch.param_defaults,
                &dispatch.param_types,
                &dispatch.defaults_module,
                static_call.id,
                static_call.span,
                ctx,
            );
            if let Some(folded) = self.fold_le_bytes_call(
                &dispatch.function_ref,
                &args,
                recorded_type,
                static_call.span,
            ) {
                return folded;
            }

            // Replay the production `Call`'s exact type args (method-level;
            // impl args ride along in `function_ref.monomorph_info`).
            let call = TirExpr::new(
                TirExprKind::Call {
                    type_args: dispatch.type_args,
                    func: Box::new(dispatch.function_ref),
                    args: CallArgs::free(args),
                },
                recorded_type,
                static_call.span,
            );
            return Self::hoist_block(call, prelude);
        }

        if let Some(converted) = self.reify_conversion_without_impl(
            static_call.id,
            &static_call.args,
            ctx,
            recorded_type,
            static_call.span,
        ) {
            return converted;
        }

        // Variant constructor in turbofish form (`Option::<T>::Some(x)`).
        if let Some(owner) = self.ann_case_owner(static_call.id) {
            return self.reify_case_construction(
                owner,
                &static_call.method,
                static_call.id,
                &static_call.args,
                None,
                static_call.span,
                ctx,
            );
        }

        // Everything else flows through `static_method_dispatch` above —
        // `resolve_static_method_call` records the resolved
        // `FunctionRef` for every non-variant static call. Hitting this
        // shape means annotate diagnosed an unresolvable call.
        TirExpr::new(TirExprKind::Unit, TypeTable::ERROR, static_call.span)
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
        args: &mut Vec<CallArg>,
        dispatch: &StaticMethodDispatch,
        site: AstId,
        span: Span,
        ctx: &mut FunctionContext,
    ) -> Vec<TirStmt> {
        let mut prelude = self.reify_pad_args_with_defaults(
            callee,
            args,
            &dispatch.param_types,
            &dispatch.function_ref.module_source.clone(),
            &dispatch.function_ref.name.clone(),
            site,
            ctx,
        );
        let module = dispatch.defaults_module.clone();
        prelude.extend(self.reify_apply_param_defaults(
            args,
            &dispatch.param_defaults,
            &dispatch.param_types,
            &module,
            site,
            span,
            ctx,
        ));
        prelude
    }

    fn reify_pad_args_with_defaults(
        &mut self,
        callee: &ast::Expr,
        args: &mut Vec<CallArg>,
        param_types: &[tir::TypeId],
        callee_module: &ModuleSource,
        callee_name: &str,
        site: AstId,
        ctx: &mut FunctionContext,
    ) -> Vec<TirStmt> {
        // Only an ident callee names a free function with defaults. A
        // namespace-qualified callee (`g::make`) is still a free function —
        // `callee_module` / `callee_name` already carry its resolved home and
        // name — so do NOT bail on `::` here; `lookup_free_func_params` returns
        // empty for anything that is not a free function in `callee_module`.
        let ast::Expr::Ident(_) = callee else {
            return Vec::new();
        };
        let func_params = self.lookup_free_func_params(callee_module, callee_name);
        self.reify_apply_param_defaults(
            args,
            &func_params,
            param_types,
            callee_module,
            site,
            callee.span(),
            ctx,
        )
    }

    /// Pad `args` with the defaults the call omitted, and answer the `let`s that
    /// must run ahead of it, for [`Self::hoist_block`].
    fn reify_apply_param_defaults(
        &mut self,
        args: &mut Vec<CallArg>,
        func_params: &[(String, Option<ast::Expr>)],
        param_types: &[tir::TypeId],
        callee_module: &ModuleSource,
        site: AstId,
        call_span: Span,
        ctx: &mut FunctionContext,
    ) -> Vec<TirStmt> {
        if !omits_a_default(args.len(), func_params) {
            return Vec::new();
        }
        // The walk annotate peeled off for *this* call. Taken before the
        // perspective swap below, which leaves the caller's module — and its
        // facts — behind. Pushed so `ann` answers with this call's type
        // arguments rather than another call site's.
        let overlay = self.ann_ref(|facts| &facts.default_overlays, site);
        assert!(
            overlay.is_some(),
            "every call site reify pads has a walk annotate left for it: \
             a site reaching here without one replays the declaration's walk, \
             where the callee's type parameters are still abstract. \
             The route that resolved this call reached neither \
             `apply_param_defaults` nor `record_default_walk`. \
             Site {site:?} in {callee_module:?} at {call_span:?}"
        );
        // Captured before the perspective swap below moves to the callee. A nested
        // defaulted call (`fn outer(x = loc())`) keeps the outermost call site.
        let call_site = self
            .call_site_location
            .clone()
            .unwrap_or_else(|| CallSiteLocation {
                module: self.current_module_source.clone(),
                span: call_span,
                function_name: ctx.function_name.clone(),
            });

        // A default resolves in the callee's scope, which may name items the caller cannot
        // see. The caller's `ctx` stays, as the frame the walk's locals live in.
        let named = slots_named_by_defaults(args.len(), func_params);
        let captured = names_captured_by_defaults(args.len(), func_params);
        let mut prelude = Vec::new();
        let mut mut_refs = MutRefPlaces::default();
        let walk = |this: &mut Self| {
            util::replaced(
                this,
                |r| &mut r.call_site_location,
                Some(call_site),
                |this| {
                    this.with_module_perspective(callee_module, |this| {
                        ctx.with_caller_bindings_hidden(|ctx| {
                            // Every argument binds ahead of the defaults, in call order. A
                            // `&mut` place stays in the call, and a default takes `&mut` of it
                            // again unless captured.
                            if named.contains(&true) {
                                for ((arg, (name, _)), named) in
                                    args.iter_mut().zip(func_params).zip(&named)
                                {
                                    let unbound = TirExpr::new(
                                        TirExprKind::Unit,
                                        TypeTable::UNIT,
                                        arg.expr.span,
                                    );
                                    let value = std::mem::replace(&mut arg.expr, unbound);
                                    arg.expr = if name == RECEIVER {
                                        this.bind_receiver_ahead(ctx, value, &mut prelude)
                                    } else if is_mut_ref(&value)
                                        && this.tysys.is_source_place(&value)
                                        && !captured.contains(name)
                                    {
                                        let mut_ref = this.bind_subscripts_ahead(
                                            ctx,
                                            value,
                                            "arg_index",
                                            &mut prelude,
                                        );
                                        if *named {
                                            let what = minted_name(name, ctx.fresh_serial());
                                            let stand_in =
                                                ctx.add_local(what, mut_ref.type_id, false, None);
                                            ctx.name_local(name.clone(), stand_in);
                                            mut_refs.0.push((stand_in, mut_ref.clone()));
                                        }
                                        mut_ref
                                    } else {
                                        this.handed_on(bind_param_to_local(
                                            ctx,
                                            name,
                                            value,
                                            &mut prelude,
                                        ))
                                    };
                                }
                            }
                            for (i, named) in named.iter().enumerate().skip(args.len()) {
                                let Some((name, Some(default_ast))) = func_params.get(i) else {
                                    break;
                                };
                                // A trait method's default has no body for annotate to walk, so
                                // without the parameter's type here it reifies untyped.
                                let expected = param_types.get(i).copied();
                                let mut resolved = this.with_expr_ownership(default_ast, |this| {
                                    this.reify_expr(default_ast, ctx, expected)
                                });
                                if let Some(expected) = expected {
                                    this.settle_packs_in_default(&mut resolved, expected);
                                }
                                mut_refs.visit_expr(&mut resolved);
                                if *named {
                                    resolved = this.handed_on(bind_param_to_local(
                                        ctx,
                                        name,
                                        resolved,
                                        &mut prelude,
                                    ));
                                }
                                // A default is a value synthesized here, with no caller storage
                                // behind it for the callee to write.
                                args.push(CallArg::new(resolved, false));
                            }
                        });
                    });
                },
            );
        };
        self.with_overlay(overlay, walk);
        prelude
    }

    /// The callee value of a call annotate recorded as indirect, read down to
    /// the function value as `build_indirect_call`'s `deref_to_value` does.
    ///
    /// `kind` says which value, so the type test that decided it at annotate
    /// time is not repeated. Only a name this frame cannot reach is left to
    /// fail, which is the two walks disagreeing.
    fn reify_indirect_callee(
        &mut self,
        callee: &ast::Expr,
        kind: IndirectCallee,
        ctx: &mut FunctionContext,
    ) -> TirExpr {
        let span = callee.span();
        let value = match (kind, callee) {
            (IndirectCallee::Binding, ast::Expr::Ident(ident)) => {
                let Some(var_ref) = ctx.lookup_or_capture(&ident.name) else {
                    unreachable!(
                        "at {}: annotate reached the binding `{}`, reify's frame cannot",
                        span.location(),
                        ident.name
                    )
                };
                var_ref_expr(var_ref, &ident.name, span)
            }
            (IndirectCallee::Global, ast::Expr::Ident(ident)) => {
                let Some((module_source, name, global_type, _mutable)) = self
                    .sem
                    .decls
                    .lookup_global(&ident.name, &self.current_module_source)
                else {
                    unreachable!(
                        "at {}: annotate read the global `{}`, reify finds no such global",
                        span.location(),
                        ident.name
                    )
                };
                TirExpr::new(
                    TirExprKind::GlobalVarGet {
                        module_source,
                        name,
                    },
                    global_type,
                    span,
                )
            }
            (IndirectCallee::Expr, _) => self.reify_expr(callee, ctx, None),
            (IndirectCallee::Binding | IndirectCallee::Global, other) => unreachable!(
                "at {}: annotate recorded a named callee, and this one has no name",
                other.span().location()
            ),
        };
        deref_to_layers(value, 0, &self.tysys.type_table.borrow())
    }

    /// Whether `func` reads `result`'s elements out of bytes as their raw bits:
    /// `builtin::array_new_data`, or `List::from_le_bytes` over a prelude impl.
    fn reads_le_bytes(&self, func: &tir::FunctionRef, result: TypeId) -> bool {
        if let Some(intrinsic) = func.intrinsic() {
            return intrinsic == ARRAY_NEW_DATA;
        }
        if !self.reads_prelude_le_bytes(result) {
            return false;
        }
        let tt = self.tysys.type_table.borrow();
        let items = tt.compiler_items();
        let Some(Resolved::Method {
            module_source,
            owner_head: Some(owner),
            name,
            ..
        }) = items.get(CompilerItem::ListFromLeBytes)
        else {
            return false;
        };
        func.module_source == *module_source
            && func.method_info.as_ref().is_some_and(|info| {
                info.trait_name.is_none()
                    && info.method_name == *name
                    && info.receiver == Receiver::Type(owner.clone())
            })
    }

    /// Whether `seq`'s element answers `FromLeBytes` with its primitive's own
    /// impl, rather than one a newtype over it writes.
    fn reads_prelude_le_bytes(&self, seq: TypeId) -> bool {
        let Some(elem) = self.tysys.type_table.borrow().seq_element(seq) else {
            return false;
        };
        let trait_ = self
            .tysys
            .compiler_trait_def(CompilerItem::FromLeBytes)
            .expect("the prelude declares `FromLeBytes`");
        self.tysys.own_impl_link(elem, trait_).is_none_or(|link| {
            matches!(
                self.tysys.type_table.borrow().get(link),
                ResolvedType::Primitive(_)
            )
        })
    }

    /// A call reading `T`s out of a byte literal, as that literal typed as the
    /// result, so lowering builds it as a constant with no decode loop.
    fn fold_le_bytes_call(
        &self,
        func: &tir::FunctionRef,
        args: &[CallArg],
        result: TypeId,
        span: Span,
    ) -> Option<TirExpr> {
        let [arg] = args else {
            return None;
        };
        let TirExprKind::BytesLiteral(bytes) = &arg.expr.kind else {
            return None;
        };
        let width = self
            .tysys
            .type_table
            .borrow()
            .packed_element(result)?
            .data_width()?;
        (bytes.len() % width == 0 && self.reads_le_bytes(func, result))
            .then(|| TirExpr::new(TirExprKind::BytesLiteral(bytes.clone()), result, span))
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
        let span = call.span;

        // Variant-ctor (`Variant::Case(payload)`) must beat the
        // `static_method_dispatch` arm: annotate also records the
        // ctor there too, but that shape would lower to a
        // `Call` against a function that doesn't exist.
        if let ast::Expr::Ident(ident) = &call.callee
            && let Some(owner) = self.ann_case_owner(ident.id)
        {
            return self.reify_case_construction(
                owner,
                ident.case_name(),
                call.id,
                &call.args,
                None,
                span,
                ctx,
            );
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
            // `$call` is dropped before codegen. Restricted to those closures
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
            self.adjust_receiver_in_args(&mut arg_exprs, &dispatch, span);
            let prelude = self.reify_pad_dispatch_defaults(
                &call.callee,
                &mut arg_exprs,
                &dispatch,
                call.id,
                span,
                ctx,
            );
            if let Some(folded) =
                self.fold_le_bytes_call(&dispatch.function_ref, &arg_exprs, recorded_type, span)
            {
                return folded;
            }
            if let Some(case_index) = self.ann_wrapped_case(call.id) {
                let [payload] = <[CallArg; 1]>::try_from(arg_exprs)
                    .unwrap_or_else(|_| unreachable!("`wrap_in_case` takes one argument"));
                let wrapped = self.wrap_in_case(recorded_type, case_index, payload.expr, span);
                return Self::hoist_block(wrapped, prelude);
            }
            // Type args: replay exactly what the production builder put on
            // the `Call`. This already folds in any explicit turbofish and,
            // crucially, carries only the method-level type args — a generic
            // struct's impl type args live in `function_ref.monomorph_info`,
            // so re-deriving from `generic_instantiations` (which is the flat
            // impl+method list) would mangle `Container<i32>::make` as
            // `Container::make<i32>` and miss the monomorphized instance.
            let call = TirExpr::new(
                TirExprKind::Call {
                    type_args: dispatch.type_args,
                    func: Box::new(dispatch.function_ref),
                    args: CallArgs::free(arg_exprs),
                },
                recorded_type,
                span,
            );
            return Self::hoist_block(call, prelude);
        }

        if let Some(converted) =
            self.reify_conversion_without_impl(call.id, &call.args, ctx, recorded_type, span)
        {
            return converted;
        }

        // Indirect call: the callee is a value rather than a named function.
        // Annotate recorded which value, so this arm builds the call it decided
        // on instead of deciding again.
        if let Some(kind) = self.ann_indirect_callee(call.id) {
            let callee_expr = self.reify_indirect_callee(&call.callee, kind, ctx);
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
            return TirExpr::new(TirExprKind::Unit, TypeTable::ERROR, span);
        }

        // A non-ident callee that is not callable — annotate already diagnosed
        // it (`TypeError::CalleeNotCallable`), and the callable ones returned
        // above. Match the elaborator's recovery shape.
        if !matches!(&call.callee, ast::Expr::Ident(_)) {
            return TirExpr::new(TirExprKind::Unit, TypeTable::ERROR, span);
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
                                func: Box::new(tir::FunctionRef {
                                    module_source: ns_source,
                                    name: rest.to_string(),
                                    template: None,
                                    monomorph_info: None,
                                    method_info: None,
                                }),
                                type_args,
                                args: CallArgs::free(arg_calls),
                            },
                            recorded_type,
                            span,
                        );
                    }
                }
                // Unresolved: emit recovery shape matching
                // annotate's diagnostic path.
                return TirExpr::new(TirExprKind::Unit, TypeTable::ERROR, span);
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
            let prelude = self.reify_pad_args_with_defaults(
                &call.callee,
                &mut args,
                &param_types,
                &callee_module,
                &callee_name,
                call.id,
                ctx,
            );

            let call = TirExpr::new(
                TirExprKind::Call {
                    func: Box::new(tir::FunctionRef {
                        module_source: callee_module,
                        name: callee_name,
                        template: self.template_at(ident.id),
                        monomorph_info: None,
                        method_info: None,
                    }),
                    type_args,
                    args: CallArgs::free(args),
                },
                recorded_type,
                span,
            );
            return Self::hoist_block(call, prelude);
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

            let owner = ident.owner_segment().map(|seg| seg.id);
            // A newtype reaches its base's constants and keeps its own type.
            let through_newtype = self.newtype_member_owner(owner, prefix);
            let flags = match through_newtype {
                Some((base, _)) => self.type_lookup().flags_members_of(base).cloned(),
                None => self.type_lookup().flags_members_at(owner, prefix).cloned(),
            };
            if let Some(flags_info) = flags
                && matches!(suffix, "none" | "all")
            {
                let value: u64 = match suffix {
                    "none" => 0,
                    "all" => u64::from(flags_info.members.iter().fold(0, |all, m| all | m.bitmask)),
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
        TirExpr::new(TirExprKind::Unit, TypeTable::ERROR, span)
    }

    /// Reify the `container[i].method(args)` `IndexMut` rewrite from two
    /// dispatch records: `operator_dispatch[index_expr.id]` for the inner
    /// `index_mut(idx)` and `method_dispatch[method_call.id]` for the outer
    /// call. Builds `container.index_mut(idx)` as the outer call's receiver.
    fn reify_index_mut_method_call(
        &mut self,
        method_call: &ast::MethodCallExpr,
        ctx: &mut FunctionContext,
        recorded_type: TypeId,
    ) -> TirExpr {
        // The AST receiver of the IndexMutMethodCall is always an
        // `Expr::Index` — guaranteed by the elaborator's
        // dispatcher; reify trusts the desugar tag's contract.
        let ast::Expr::Index(index_expr) = &method_call.receiver else {
            panic!(
                "reify_index_mut_method_call: receiver is not an IndexExpr (desugar invariant violated)"
            );
        };

        let inner_dispatch = self
            .index_read_dispatch(index_expr)
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
        let receiver_for_index_mut =
            self.adjust_receiver(container, inner_dispatch.self_kind, false, ctx);
        let index_resolved = self.reify_expr(&index_expr.index, ctx, None);
        let index_mut_call = build_tir_method_call(
            receiver_for_index_mut,
            inner_dispatch.function_ref,
            vec![],
            vec![CallArg::new(index_resolved, false)],
            inner_dispatch.return_type,
            index_expr.span,
        );

        self.reify_dispatched_method_call(
            method_call,
            index_mut_call,
            outer_dispatch,
            recorded_type,
            ctx,
        )
    }

    /// Reify a `MethodCallExpr`. Every decision — resolved `FunctionRef`,
    /// receiver-adjustment kind, ref-impl flag, final type — is already on
    /// `sem.types`. An `IndexMutMethodCall` desugar routes through here too,
    /// materialising `$index_mut_val` before the dispatch.
    fn reify_method_call(
        &mut self,
        method_call: &ast::MethodCallExpr,
        ctx: &mut FunctionContext,
        recorded_type: TypeId,
    ) -> TirExpr {
        // IndexMut rewrite gets first crack — when the elaborator
        // tagged this call as `IndexMutMethodCall`, the receiver is an
        // index expression that needs `$index_mut_val` synthesis.
        if matches!(
            self.ann_desugars(method_call.id),
            Some(DesugarKind::IndexMutMethodCall)
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
        let raw_receiver = self.reify_expr(&method_call.receiver, ctx, None);
        if matches!(method_call.method.as_str(), "len" | "zip") {
            // Auto-deref through `&`/`&mut` so tuple `.len()` / `.zip()` work
            // on a reference receiver, like any other method (mirrors the
            // elaborator's `get_base_type`). The `receiver` expr is kept as-is
            // for field access, which auto-derefs.
            let base_type_id = self
                .tysys
                .type_table
                .borrow()
                .peel_refs(raw_receiver.type_id);
            let is_tuple_receiver = self.tysys.type_table.borrow().is_tuple(base_type_id);
            if is_tuple_receiver {
                let receiver = raw_receiver;
                return match method_call.method.as_str() {
                    "len" => {
                        // A tuple type still carrying a `..T` pack has an arity
                        // unknown until monomorphization; defer folding to a
                        // literal via `TupleLen` so it is not frozen at the
                        // unsubstituted pack count (mirrors the `zip` deferral).
                        if self
                            .tysys
                            .type_table
                            .borrow()
                            .contains_type_pack(base_type_id)
                        {
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
                        let span = method_call.span;
                        // Bound ahead of the branch so the deferred expansion
                        // inherits it: the monomorphizer allocates no locals.
                        let mut stmts = Vec::new();
                        let receiver = Self::hoist_once(ctx, receiver, "zip", &mut stmts);
                        // A concrete tuple-of-tuples transposes inline here;
                        // only a type-pack receiver defers expansion to the
                        // monomorphiser via `TupleZip`. Non-generic bodies
                        // never reach the monomorphiser, so emitting
                        // `TupleZip` here would hit `lower::translate`'s
                        // `unreachable!`.
                        let transposed = if self
                            .tysys
                            .type_table
                            .borrow()
                            .contains_type_pack(base_type_id)
                        {
                            TirExpr::new(
                                TirExprKind::TupleZip {
                                    expr: Box::new(receiver),
                                },
                                recorded_type,
                                span,
                            )
                        } else {
                            transpose_tuple_expr(
                                &receiver,
                                span,
                                &mut self.tysys.type_table.borrow_mut(),
                            )
                        };
                        Self::hoist_block(transposed, stmts)
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
        self.reify_dispatched_method_call(method_call, raw_receiver, dispatch, recorded_type, ctx)
    }

    /// The call `dispatch` names on `raw_receiver`, its arguments walked and
    /// its omitted defaults filled.
    fn reify_dispatched_method_call(
        &mut self,
        method_call: &ast::MethodCallExpr,
        raw_receiver: TirExpr,
        dispatch: MethodDispatch,
        recorded_type: TypeId,
        ctx: &mut FunctionContext,
    ) -> TirExpr {
        // An argument past the dispatched signature, a recovery shape, is not
        // `&mut`.
        let mut args: Vec<CallArg> = method_call
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
                CallArg::new(arg_tir, is_mut)
            })
            .collect();

        let mut prelude = self.reify_apply_param_defaults(
            &mut args,
            &dispatch.param_defaults,
            &dispatch.param_types,
            &dispatch.defaults_module,
            method_call.id,
            method_call.span,
            ctx,
        );
        let raw_receiver = if prelude.is_empty() {
            raw_receiver
        } else {
            let mut bound = Vec::new();
            let receiver = self.bind_receiver_ahead(ctx, raw_receiver, &mut bound);
            bound.append(&mut prelude);
            prelude = bound;
            receiver
        };

        let raw_receiver = if dispatch.through_referent {
            wrapped_reference(raw_receiver, &self.tysys.type_table.borrow())
        } else {
            raw_receiver
        };
        let adjusted_receiver =
            self.adjust_receiver(raw_receiver, dispatch.self_kind, dispatch.is_ref_impl, ctx);

        // Monomorphization queues instances off exactly what annotate resolved;
        // `monomorph_info.method_type_args` is zeroed for a blanket-impl turbofish.
        let type_args = dispatch.method_type_args.clone();

        // The per-`AstId` type can be wrong at a call site, making a unit call
        // look value-producing and emitting a `drop` of nothing.
        let result_type = if dispatch.return_type == TypeTable::UNKNOWN {
            recorded_type
        } else {
            dispatch.return_type
        };
        let call = build_tir_method_call(
            adjusted_receiver,
            dispatch.function_ref,
            type_args,
            args,
            result_type,
            method_call.span,
        );
        Self::hoist_block(call, prelude)
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
        let resolved = self.tysys.type_table.borrow().get(receiver_type).clone();
        // The receiver's own head answers: same-named structs in different
        // modules reach their own fields, and an anonymous shape — which no
        // spelling names — reaches its own.
        let (head, type_args): (Option<StructDef>, Vec<TypeId>) = match resolved {
            ResolvedType::Struct { def, .. } => (Some(def), vec![]),
            ResolvedType::GenericInstance { type_args, .. } => {
                // Tuple projection (`t.0`): a tuple has no struct decl, so the
                // struct-fields lookup below misses and the `(0, …)` fallback
                // would collapse every `t.N` onto field 0. Resolve the numeric
                // field name into the element index directly.
                let is_tuple = self.tysys.type_table.borrow().is_tuple(receiver_type);
                if is_tuple
                    && let Ok(index) = field_name.parse::<usize>()
                    && let Ok(elem) = self.tysys.tuple_literal_index_type(&type_args, index)
                {
                    return (index as u32, field_name.to_string(), Some(elem));
                }
                (
                    self.tysys.type_def(receiver_type).map(StructDef::Decl),
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
            let substitution: hashmap::IndexMap<u32, TypeId> = (0..type_args.len() as u32)
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
    fn with_module_perspective<R>(
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
                (Some(m), Some(sem)) => Some((m.items.as_slice(), &**sem)),
                _ => None,
            }
        };
        match swap {
            Some((items, sem)) => {
                self.with_perspective(module.clone(), items, sem, &sem.types, body)
            }
            None => body(self),
        }
    }

    /// Run `body` standing in another module: its source, its items, the scope
    /// its names resolve in and the facts of the walk replayed, all restored on
    /// return.
    fn with_perspective<R>(
        &mut self,
        module: ModuleSource,
        items: &'a [Item],
        sem: &'a ModuleSemantics,
        types: &'a TypeAnnotations,
        body: impl FnOnce(&mut Self) -> R,
    ) -> R {
        util::replaced(
            self,
            |r| &mut r.current_module_source,
            module,
            |r| {
                util::replaced(
                    r,
                    |r| &mut r.current_module_items,
                    items,
                    |r| {
                        util::replaced(
                            r,
                            |r| &mut r.sem,
                            sem,
                            |r| util::replaced(r, |r| &mut r.types, types, body).0,
                        )
                        .0
                    },
                )
                .0
            },
        )
        .0
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
        let canonical_ident;
        let ident = if let Some(name) = self.sem.imports.canonical_ns_ref(&ident.name) {
            canonical_ident = ast::IdentExpr {
                name,
                ..ident.clone()
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
            return var_ref_expr(var_ref, &ident.name, ident.span);
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
            // sanctions (WEP 2026-05-26 §"Reify — mechanical").
            let ty = self.resolve_global_type(&global_decl.ty);
            return TirExpr::new(
                TirExprKind::GlobalVarGet {
                    module_source: self.current_module_source.clone(),
                    name: ident.name.clone(),
                },
                ty,
                ident.span,
            );
        }

        // 4. Associated constant (e.g. `f64::PI`, `i32::MAX`).
        if let Some(assoc) = self.tysys.associated_constant_of_path(ident) {
            return self.inline_associated_const(assoc, ident.span, ctx);
        }

        // 5. A case path, in `resolve_qualified_case`'s order. Its recorded type
        //    is the newtype its prefix named, or the case's own.
        if let Some(owner) = self.ann_case_owner(ident.id) {
            let case_name = ident.case_name();
            let lookup = self.type_lookup();
            if lookup.variant_cases_of(owner).is_some() {
                return self.reify_case_construction(
                    owner,
                    case_name,
                    ident.id,
                    &[],
                    expected_type,
                    ident.span,
                    ctx,
                );
            }
            if let Some(enum_info) = lookup.enum_cases_of(owner)
                && let Some(case_data) = enum_info.find_case(case_name)
            {
                let enum_type = self
                    .tysys
                    .type_table
                    .borrow()
                    .type_id_of_decl(enum_info.defined_at);
                return TirExpr::new(
                    TirExprKind::EnumConstruct {
                        enum_type,
                        case_index: case_data.index,
                        case_name: case_data.name.clone(),
                    },
                    recorded_type,
                    ident.span,
                );
            }
            let member = lookup
                .flags_members_of(owner)
                .and_then(|flags| flags.members.iter().find(|m| m.name == case_name))
                .expect("a case owner declares the case annotate resolved");
            return TirExpr::new(
                TirExprKind::IntLiteral {
                    value: u64::from(member.bitmask),
                    repr: member.bitmask.to_string(),
                },
                recorded_type,
                ident.span,
            );
        }

        // 6. Free function reference — the ident names a function in
        //    the current module or imported via a `use` declaration.
        //    Emit `TirExprKind::FuncRef` with the recorded
        //    instantiation's type_args when present.
        if self
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
                    template: self.template_at(ident.id),
                },
                recorded_type,
                ident.span,
            );
        }
        if let Some(def) = self.tysys.resolutions.declared_if_walked(ident.id) {
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
                    template: Some(self.tysys.signatures.declared_template(def)),
                },
                recorded_type,
                ident.span,
            );
        }

        // 6b. Imported free function reference resolved through the symbol
        //     table (covers namespace-import functions, whose `ns$fn` aliases
        //     name functions rather than types). Mirrors annotate's
        //     `resolve_func_ref_ident` → `lookup_func_ast_for_ref` and emits a
        //     `FuncRef` keyed by the function's defining module + original name.
        if self.sem.decls.imported_functions.contains(&ident.name)
            && let Some(symbol) = self.symbol_at(ident.id)
            && matches!(symbol.kind, SymbolKind::Function(_))
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
                    template: self.template_at(ident.id),
                },
                recorded_type,
                ident.span,
            );
        }

        // No remaining recognised ident kind — the elaborator would
        // have diagnosed an unknown identifier at annotate time.
        // Match the elaborator's recovery shape so reify doesn't
        // panic on a known-bad input.
        TirExpr::new(TirExprKind::Unit, TypeTable::ERROR, ident.span)
    }

    /// Replay an `i128` / `u128` numeric-literal coercion recorded by annotate,
    /// returning `None` for every other shape. The 128-bit types are prelude
    /// structs, so the value is materialized by a constructor call; every other
    /// `NumericLiteral` coercion is free, the literal already carrying its
    /// coerced type.
    fn try_reify_int128_coercion(&self, expr: &ast::Expr) -> Option<TirExpr> {
        let choice = self.ann_coercions(expr.id())?;
        if choice.kind != CoercionKind::NumericLiteral {
            return None;
        }
        let target_type = choice.target_type;
        let item = self.tysys.type_table.borrow().wide_int_item(target_type)?;

        // Every shape the coercion admits, the negated `-NUM` among them, whose
        // coercion is keyed on the enclosing `Unary` node. Reading the one
        // classifier is what keeps a shape from arriving here as a bare
        // `IntLiteral` typed as the `i128` struct.
        let literal = classify_numeric_literal(expr)?;
        let negated = literal.neg.is_some();
        let repr = match literal.kind {
            NumericLiteralKind::Number(repr) => repr.to_string(),
            NumericLiteralKind::Byte(raw) => unescape_byte(raw).ok()?.to_string(),
        };

        let parse_result = if item == CompilerItem::U128 {
            parse_u128_literal(&repr).map(|v| v as i128)
        } else if negated {
            parse_i128_literal(&format!("-{repr}"))
        } else {
            parse_i128_literal(&repr)
        };
        let value = parse_result.ok()?;
        Some(create_literal(
            item,
            value,
            target_type,
            &self.tysys.type_table.borrow(),
            expr.span(),
        ))
    }

    /// `inner as target_type`, `inner` already read through its references:
    /// the prelude calls a half or a wide integer needs, then the bare `Cast`.
    fn lower_cast(&self, inner: TirExpr, target_type: TypeId, span: Span) -> TirExpr {
        if self.is_newtype_step(inner.type_id, target_type) {
            return bare_cast(inner, target_type, span);
        }
        let inner = match self.lower_half_cast(inner, target_type, span) {
            ControlFlow::Break(lowered) => return lowered,
            ControlFlow::Continue(inner) => inner,
        };
        let inner = match self.lower_int128_cast(inner, target_type, span) {
            ControlFlow::Break(lowered) => return lowered,
            ControlFlow::Continue(inner) => inner,
        };
        let (from_handle, to_handle) = {
            let tt = self.tysys.type_table.borrow();
            (
                tt.is_unrestricted_handle(inner.type_id),
                tt.is_unrestricted_handle(target_type),
            )
        };
        let inner = match (from_handle, to_handle) {
            (true, false) => handle_to_f64(inner),
            (false, true) => return handle_from_f64(inner, target_type),
            _ => inner,
        };
        bare_cast(inner, target_type, span)
    }

    /// Whether `source as target` changes no representation: both share one,
    /// or either is still generic, where the checker admits only a newtype
    /// step (`cast_needs_no_judgment`).
    fn is_newtype_step(&self, source: TypeId, target: TypeId) -> bool {
        let tt = self.tysys.type_table.borrow();
        tt.share_common_base(source, target)
            || tt.representation_awaits_substitution(source)
            || tt.representation_awaits_substitution(target)
    }

    /// A cast with `f16` or `bf16` on either side. No instruction converts
    /// either, so a half widens to its exact `f32` and casts on from there,
    /// and a number reaches a half through the prelude conversion that rounds
    /// it once. `Continue` hands `inner` on where neither side is a half.
    fn lower_half_cast(
        &self,
        inner: TirExpr,
        target_type: TypeId,
        span: Span,
    ) -> ControlFlow<TirExpr, TirExpr> {
        let (source_half, target_half) = {
            let tt = self.tysys.type_table.borrow();
            let half = |id| tt.primitive_head(id).filter(|p| p.is_half());
            (half(inner.type_id), half(target_type))
        };
        if source_half.is_none() && target_half.is_none() {
            return ControlFlow::Continue(inner);
        }
        let source = match source_half {
            Some(half) => widen_half(&self.tysys.type_table.borrow(), inner, half, span),
            None => inner,
        };
        let Some(half) = target_half else {
            return ControlFlow::Break(self.lower_cast(source, target_type, span));
        };
        let (base, item, via) = {
            let tt = self.tysys.type_table.borrow();
            let source_base = tt.representation_head(source.type_id);
            match (
                half,
                tt.primitive_head(source_base),
                tt.wide_int_item(source_base),
            ) {
                (PrimitiveType::F16, _, _) => {
                    (TypeTable::F16, CompilerItem::F16FromF64, TypeTable::F64)
                }
                (_, _, Some(CompilerItem::I128)) => {
                    (TypeTable::BF16, CompilerItem::Bf16FromI128, source_base)
                }
                (_, _, Some(_)) => (TypeTable::BF16, CompilerItem::Bf16FromU128, source_base),
                (_, Some(PrimitiveType::I64), _) => {
                    (TypeTable::BF16, CompilerItem::Bf16FromI64, TypeTable::I64)
                }
                (_, Some(PrimitiveType::U64), _) => {
                    (TypeTable::BF16, CompilerItem::Bf16FromU64, TypeTable::U64)
                }
                _ => (TypeTable::BF16, CompilerItem::Bf16FromF64, TypeTable::F64),
            }
        };
        let arg = self.lower_cast(source, via, span);
        let converted = rt_call(&self.tysys.type_table.borrow(), item, arg, base, span);
        ControlFlow::Break(bare_cast(converted, target_type, span))
    }

    /// A cast with a wide integer on either side, `inner` already read through
    /// its references. The 128-bit types are prelude structs, so a bare `Cast`
    /// would leak the boxed struct ref into a slot expecting a Wasm scalar
    /// (issue #1328), or a scalar into one expecting the struct. `Continue` hands
    /// `inner` on to the bare cast where neither side needs a lowering.
    fn lower_int128_cast(
        &self,
        inner: TirExpr,
        target_type: TypeId,
        span: Span,
    ) -> ControlFlow<TirExpr, TirExpr> {
        let tt = self.tysys.type_table.borrow();
        if tt.is_wide_int(inner.type_id) {
            drop(tt);
            return self.lower_int128_source_cast(inner, target_type, span);
        }
        let Some(item) = tt.wide_int_item(target_type) else {
            return ControlFlow::Continue(inner);
        };
        ControlFlow::Break(create_conversion(item, inner, target_type, &tt, span))
    }

    /// `i128/u128 as T`: `f64`/`f32` through the correctly rounded
    /// `as_f64`/`as_f32`, integer targets through `low()` plus a primitive
    /// cast (truncation), and `i128 ↔ u128` through the bit-reinterpreting
    /// `from_u128`/`from_i128` constructors. Any other target hands `inner`
    /// on; `resolve_cast` has already reported it if it is invalid.
    fn lower_int128_source_cast(
        &self,
        inner: TirExpr,
        target_type: TypeId,
        span: Span,
    ) -> ControlFlow<TirExpr, TirExpr> {
        let source_type = inner.type_id;
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
        let (source_item, target_item) = {
            let tt = self.tysys.type_table.borrow();
            (
                tt.wide_int_item(source_base)
                    .expect("the caller checked the source is a wide integer"),
                tt.wide_int_item(target_base),
            )
        };
        let signed_source = source_item == CompilerItem::I128;

        enum Lowering {
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
            _ => match target_item {
                Some(item) => {
                    assert_ne!(
                        item, source_item,
                        "`lower_cast` takes a cast within one base as a newtype step"
                    );
                    Lowering::Reinterpret(if signed_source {
                        CompilerItem::U128FromI128
                    } else {
                        CompilerItem::I128FromU128
                    })
                }
                None => return ControlFlow::Continue(inner),
            },
        };

        // A newtype source first steps to its wide base so the prelude calls
        // below see their declared receiver/argument type.
        let inner = bare_cast(inner, source_base, span);
        match lowering {
            Lowering::Method(item) => {
                let func = method_ref(&self.tysys.type_table.borrow(), item);
                let receiver = adjust_receiver_for_self_kind(
                    inner,
                    ast::SelfKind::Ref,
                    /* is_ref_impl */ false,
                    &self.tysys.type_table,
                );
                let call = build_tir_method_call(receiver, func, vec![], vec![], target_base, span);
                ControlFlow::Break(bare_cast(call, target_type, span))
            }
            Lowering::LowThenCast => {
                let item = if signed_source {
                    CompilerItem::I128Low
                } else {
                    CompilerItem::U128Low
                };
                let func = method_ref(&self.tysys.type_table.borrow(), item);
                let receiver = adjust_receiver_for_self_kind(
                    inner,
                    ast::SelfKind::Ref,
                    /* is_ref_impl */ false,
                    &self.tysys.type_table,
                );
                let low_call =
                    build_tir_method_call(receiver, func, vec![], vec![], TypeTable::U64, span);
                let converted = bare_cast(low_call, target_base, span);
                ControlFlow::Break(bare_cast(converted, target_type, span))
            }
            Lowering::Reinterpret(item) => {
                let func = method_ref(&self.tysys.type_table.borrow(), item);
                let call = unary_call(func, inner, target_base, span);
                ControlFlow::Break(bare_cast(call, target_type, span))
            }
        }
    }

    fn reify_literal(
        &mut self,
        lit: &ast::LiteralExpr,
        recorded_type: TypeId,
        ctx: &FunctionContext,
    ) -> TirExpr {
        let kind = match &lit.value {
            ast::Literal::Number(repr) => {
                return self.reify_numeric_literal(repr, recorded_type, lit.span);
            }
            // A byte literal is an integer literal spelled as a character, so
            // it takes the same route by its decimal spelling — `let x: f64 =
            // b'A'` and `let x: i128 = b'A'` reach the float and wide-integer
            // readings a decimal literal does. `u8` is its type on its own.
            ast::Literal::Byte(raw) => {
                let byte = unescape_byte(raw).unwrap_or(0);
                let byte_type = if recorded_type == TypeTable::UNKNOWN {
                    TypeTable::U8
                } else {
                    recorded_type
                };
                return self.reify_numeric_literal(&byte.to_string(), byte_type, lit.span);
            }
            ast::Literal::String(s) => {
                // Decode escape sequences (`\"`, `\n`, `\\`, …) the same
                // way the elaborator does — the AST holds
                // the raw source text. Without this a literal like
                // `"{\""` reaches codegen with the backslash intact and
                // serializes as `{\"` instead of `{"`.
                let value = unescape_string(s).unwrap_or_default();
                TirExprKind::StringLiteral(value)
            }
            ast::Literal::Bytes(raw) => {
                // Decode the raw source to bytes and reuse the `#include_bytes`
                // lowering (`BytesLiteral` -> byte-buffer data segment).
                let bytes = unescape_bytes(raw).unwrap_or_default();
                let byte_list_type = if recorded_type == TypeTable::UNKNOWN {
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
                let ch = unescape_char(s).unwrap_or('\0');
                TirExprKind::CharLiteral(ch)
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
                TirExprKind::StringLiteral(display_function_name(&name))
            }
            ast::Literal::LocationFile => {
                // `#file`; in a default, the caller's module.
                let string_type = self
                    .tysys
                    .type_table
                    .borrow_mut()
                    .make_compiler_struct(CompilerItem::String);
                let file = match &self.call_site_location {
                    Some(loc) => loc.module.to_string(),
                    None => self
                        .tysys
                        .trait_env
                        .parse_modules()
                        .module_of_span(lit.span, &self.current_module_source)
                        .to_string(),
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
                    TypeTable::I32,
                    lit.span,
                );
            }
            ast::Literal::DataSection => {
                // `#data` — the loaded module's `__DATA__` section.
                let string_type = self
                    .tysys
                    .type_table
                    .borrow_mut()
                    .make_compiler_struct(CompilerItem::String);
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
                    .make_compiler_struct(CompilerItem::String);
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
                let array_u8_type = if recorded_type == TypeTable::UNKNOWN {
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

    /// A numeric literal as TIR, from the decimal / hex / float spelling every
    /// such literal reduces to. `repr` rides along because later readers parse
    /// the value back out of it — `i128` / `u128` lowering is one.
    fn reify_numeric_literal(&mut self, repr: &str, recorded_type: TypeId, span: Span) -> TirExpr {
        // The *recorded type* decides Int vs Float TIR literal; the literal's
        // *syntactic form* (`defaults_to_float`) decides how to read its
        // value, so `let x: f64 = 0xFF` reads as an integer then converts. Peel
        // newtypes first, or a float literal bound to a float-newtype target
        // takes the integer path.
        let base_target = self
            .tysys
            .type_table
            .borrow()
            .representation_head(recorded_type);
        // A half has no float value of its own: its literal is its bits.
        if let Some(format) = self.tysys.half_format(base_target) {
            let bits = float_literal_bits(repr, format).unwrap_or(0);
            return half_literal(bits, recorded_type, span);
        }
        // A float-only literal (`1.0`, `0.0`, `1e2`) is a float regardless of
        // the recorded type: when the recorded type is missing/UNKNOWN (e.g. a
        // stdlib const body whose `expression_types` entry is absent from the
        // cached snapshot) the syntactic form is authoritative, matching
        // production's `resolve_numeric_literal`. An integer literal still
        // defers to the recorded type so `let x: f64 = 1` takes the float path
        // via `is_float_target`.
        let is_float_target = base_target == TypeTable::F32
            || base_target == TypeTable::F64
            || (recorded_type == TypeTable::UNKNOWN && defaults_to_float(repr));
        if is_float_target {
            // Rounded once, into the target format, so the later narrowing
            // of an `f32` is exact.
            let format = if base_target == TypeTable::F32 {
                FloatFormat::F32
            } else {
                FloatFormat::F64
            };
            let value = format.value(float_literal_bits(repr, format).unwrap_or(0));
            // The literal's *type* must be a concrete float, not the (possibly
            // UNKNOWN) recorded type: a float-only literal with no recorded
            // type defaults to `f64` (matching production's
            // `resolve_numeric_literal`). Leaving it UNKNOWN makes lowering
            // pick an integer op for the surrounding arithmetic (`f64.div` ->
            // `i32.div_s`). f32 target keeps f32; otherwise (f64 target, or no
            // recorded type) an untyped float literal defaults to f64.
            let float_type = if base_target == TypeTable::F32 {
                TypeTable::F32
            } else {
                TypeTable::F64
            };
            return TirExpr::new(
                TirExprKind::FloatLiteral {
                    value,
                    repr: repr.to_string(),
                },
                float_type,
                span,
            );
        }
        let value = util::parse_u128_literal(repr).unwrap_or(0) as u64;
        TirExpr::new(
            TirExprKind::IntLiteral {
                value,
                repr: repr.to_string(),
            },
            recorded_type,
            span,
        )
    }

    /// An associated constant's body, inlined where it is read. Its `AstId`s
    /// index its defining module, and are unique only within one, so it is
    /// reified under that module's perspective. Its names resolve there too,
    /// never to a binding of the function it lands in, while its locals live
    /// in that function's frame.
    fn inline_associated_const(
        &mut self,
        assoc: AssocConstSig,
        span: Span,
        ctx: &mut FunctionContext,
    ) -> TirExpr {
        let AssocConstSig {
            module, ty, value, ..
        } = assoc;
        let resolved = ctx.with_caller_bindings_hidden(|ctx| {
            self.with_module_perspective(&module, |this| {
                this.with_expr_ownership(&value, |this| this.reify_expr(&value, ctx, Some(ty)))
            })
        });
        TirExpr::new(resolved.kind, ty, span)
    }

    /// The value of the constant `pattern` names, as annotate's
    /// `resolve_pattern_constant` resolved it: an immutable global, bare or
    /// under a namespace, or an associated constant inlined. `None` where it
    /// names none.
    fn reify_pattern_constant(
        &mut self,
        pattern: &ast::Pattern,
        ctx: &mut FunctionContext,
    ) -> Option<TirExpr> {
        match pattern {
            ast::Pattern::Ident { name, span, .. } => self.immutable_global_expr(name, *span),
            ast::Pattern::Variant {
                variant_name,
                variant_qualifier,
                bindings,
                span,
                ..
            } if bindings.is_empty() => {
                if let Some(assoc) = self
                    .tysys
                    .associated_constant_qualified(variant_qualifier.as_ref(), variant_name)
                {
                    return Some(self.inline_associated_const(assoc, *span, ctx));
                }
                let alias = self
                    .sem
                    .imports
                    .pattern_ns_member(variant_qualifier.as_ref(), variant_name)?;
                self.immutable_global_expr(&alias, *span)
            }
            _ => None,
        }
    }

    /// The pattern matching `constant`'s value. A literal folds to a `Literal`
    /// pattern, an integer signed per the scrutinee, so it benefits from switch
    /// lowering; anything else is a `ConstantValue`.
    fn constant_value_pattern(&self, constant: TirExpr, scrutinee_type: TypeId) -> TirPattern {
        match &constant.kind {
            TirExprKind::IntLiteral { repr, .. } => {
                let is_unsigned = self
                    .tysys
                    .type_table
                    .borrow()
                    .is_unsigned_int(scrutinee_type);
                if is_unsigned {
                    if let Ok(v) = parse_u128_literal(repr) {
                        return TirPattern::Literal(TirLiteralPattern::U128(v));
                    }
                } else if let Ok(v) = parse_i128_literal(repr) {
                    return TirPattern::Literal(TirLiteralPattern::I128(v));
                }
            }
            TirExprKind::BoolLiteral(v) => {
                return TirPattern::Literal(TirLiteralPattern::Bool(*v));
            }
            TirExprKind::CharLiteral(v) => {
                return TirPattern::Literal(TirLiteralPattern::Char(*v));
            }
            _ => {}
        }
        TirPattern::ConstantValue {
            expr: Box::new(constant),
        }
    }

    /// A range with a constant bound, as annotate's `resolve_constant_range`
    /// resolved it: the scrutinee held in the local annotate reserved,
    /// matching where `start <= it` and `it < end` (`<=` where `inclusive`)
    /// hold by the type's order.
    fn reify_constant_range(
        &mut self,
        bounds: [&ast::Pattern; 2],
        inclusive: bool,
        shown: String,
        scrutinee_type: TypeId,
        ctx: &mut FunctionContext,
    ) -> TirPattern {
        let scrutinee = self.tysys.type_table.borrow().peel_refs(scrutinee_type);
        let mut constants = bounds.map(|bound| self.reify_pattern_constant(bound, ctx));
        let (origin, span) = bounds
            .into_iter()
            .zip(&constants)
            .find_map(|(bound, constant)| constant.is_some().then(|| constant_pattern_site(bound)))
            .expect("annotate takes a range with no constant bound as no range");
        let [start, end] = [0, 1].map(|i| {
            if let Some(constant) = constants[i].take() {
                return constant;
            }
            let bound = range_bound(bounds[i], &self.tysys.resolutions)
                .and_then(Result::ok)
                .expect("annotate reports a bound naming nothing");
            let value = bound_value(&bound, scrutinee, &mut self.tysys.type_table.borrow_mut());
            let Ok(value) = value else {
                unreachable!("annotate reports a bound naming no value of the scrutinee");
            };
            bound_literal(value, scrutinee, &self.tysys.type_table.borrow(), span)
        });
        let local_index = ctx.add_local(range_local_name(), scrutinee, false, None);
        let value = TirExpr::new(
            TirExprKind::Local {
                index: local_index,
                name: range_local_name(),
            },
            scrutinee,
            span,
        );
        let below_end = if inclusive {
            ast::BinaryOp::LtEq
        } else {
            ast::BinaryOp::Lt
        };
        let from_start = self.reify_binary_op(
            origin,
            ast::BinaryOp::LtEq,
            start,
            value.clone(),
            TypeTable::BOOL,
            span,
        );
        let to_end = self.reify_binary_op(origin, below_end, value, end, TypeTable::BOOL, span);
        TirPattern::Narrow {
            name: None,
            local_index,
            type_id: scrutinee,
            test: Box::new(TirExpr::new(
                TirExprKind::Binary {
                    op: TirBinaryOp::And,
                    left: Box::new(from_start),
                    right: Box::new(to_end),
                },
                TypeTable::BOOL,
                span,
            )),
            shown,
        }
    }

    /// `pattern` on `scrutinee_type`: lowered now when the type is settled,
    /// left to each instance when it is still a type parameter.
    fn literal_value_pattern(
        &self,
        pattern: InstancePattern,
        scrutinee_type: TypeId,
        site: Span,
        ctx: &mut FunctionContext,
    ) -> TirPattern {
        let mut type_table = self.tysys.type_table.borrow_mut();
        if settles_literal_patterns(&type_table, scrutinee_type) {
            let lowered = lower_instance_pattern(
                &pattern,
                scrutinee_type,
                &mut type_table,
                |name, ty| ctx.add_local(name, ty, false, None),
                site,
            );
            let Ok(lowered) = lowered else {
                unreachable!(
                    "annotate reports a pattern naming no value, so reify skips the module"
                );
            };
            lowered
        } else {
            TirPattern::PerInstance {
                pattern,
                scrutinee_type,
                span: site,
            }
        }
    }

    /// [`super::types::newtype_member_owner`] for the declaration `prefix`
    /// names at `site`.
    pub(super) fn newtype_member_owner(
        &self,
        site: Option<ast::AstId>,
        prefix: &str,
    ) -> Option<(DefId, TypeId)> {
        let lookup = self.type_lookup();
        let def = lookup.declaration_at(site, prefix)?;
        newtype_member_owner(&lookup, &self.tysys, def)
    }

    /// Discriminant index of `case_name` when `scrutinee_type` is an enum that
    /// declares it. Drives lowering an enum-case pattern to `TirPattern::Enum`.
    fn scrutinee_enum_case_index(&self, scrutinee_type: TypeId, case_name: &str) -> Option<u32> {
        // Peel references for match ergonomics: `match &c { Red => … }`
        // presents the scrutinee as `&Color`.
        let peeled = self
            .tysys
            .type_table
            .borrow()
            .scrutinee_structure_head(scrutinee_type);
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
        let peeled = self
            .tysys
            .type_table
            .borrow()
            .scrutinee_structure_head(scrutinee_type);
        if !matches!(
            self.tysys.type_table.borrow().get(peeled),
            ResolvedType::Variant { .. } | ResolvedType::GenericInstance { .. }
        ) {
            return false;
        }
        self.tysys
            .type_def(peeled)
            .and_then(|def| self.type_lookup().variant_cases_of(def))
            .is_some_and(|info| info.case_named(case_name).is_some())
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
        // The head the cases come from, peeled the way `scrutinee_has_variant_case`
        // already asks: references for match ergonomics, then newtypes.
        let head = self
            .tysys
            .type_table
            .borrow()
            .scrutinee_structure_head(scrutinee_type);
        let (case_index, payload_type) = self.variant_case_index_and_payload(head, case_name);
        TirPattern::Variant {
            enum_type: head,
            variant_name: case_name.to_string(),
            case_index: resolved_case_index(case_index, case_name),
            bindings: vec![],
            payload_type,
        }
    }

    /// A read of the immutable global `name`, defined here or imported. A
    /// mutable global is no constant, so its name binds in a pattern.
    fn immutable_global_expr(&self, name: &str, span: Span) -> Option<TirExpr> {
        let (module_source, name, ty) = match self.sem.decls.current_module_globals.get(name) {
            Some(&(ty, false)) => (self.current_module_source.clone(), name.to_string(), ty),
            Some(_) => return None,
            None => match self.sem.decls.imported_globals.get(name)? {
                (source_module, original_name, ty, false) => {
                    (source_module.clone(), original_name.clone(), *ty)
                }
                _ => return None,
            },
        };
        Some(TirExpr::new(
            TirExprKind::GlobalVarGet {
                module_source,
                name,
            },
            ty,
            span,
        ))
    }

    /// A constant pattern whose `==` annotate dispatched to `Eq`: the scrutinee
    /// held in the local annotate reserved, matching where the call holds.
    fn compare_constant_by_eq(
        &mut self,
        written: &ast::Pattern,
        pattern: TirPattern,
        scrutinee_type: TypeId,
        ctx: &mut FunctionContext,
    ) -> TirPattern {
        let TirPattern::ConstantValue { expr: constant } = &pattern else {
            return pattern;
        };
        let (pattern_id, _) = constant_pattern_site(written);
        let Some(dispatch) = self.ann_operator_dispatch(pattern_id) else {
            return pattern;
        };
        let span = constant.span;
        let name = constant_pattern_local_name();
        let local_index = ctx.add_local(name.clone(), scrutinee_type, false, None);
        let scrutinee = TirExpr::new(
            TirExprKind::Local {
                index: local_index,
                name,
            },
            scrutinee_type,
            span,
        );
        let test = self.binary_operator_call(dispatch, scrutinee, (**constant).clone(), span);
        TirPattern::Narrow {
            name: None,
            local_index,
            type_id: scrutinee_type,
            test: Box::new(test),
            shown: unparse_pattern_source(written),
        }
    }

    /// `let [a, b] = value;`. Under `let mut`, every binding is mutable: each
    /// local carries its own mutability, which is all a later phase reads.
    fn reify_let_destructure(
        &mut self,
        pattern: &ast::Pattern,
        value_type: TypeId,
        is_mut: bool,
        value: TirExpr,
        span: Span,
        ctx: &mut FunctionContext,
    ) -> TirStmt {
        let pattern = self.reify_pattern(pattern, value_type, span, ctx);
        if is_mut {
            for (name, index, _) in collect_pattern_bindings_with_index(&pattern) {
                ctx.make_mutable(&name, index);
            }
        }
        TirStmt::new(TirStmtKind::LetDestructure { pattern, value }, span)
    }

    /// `site` is where a diagnostic about the pattern points: the arm or
    /// statement that holds it.
    pub(super) fn reify_pattern(
        &mut self,
        pattern: &ast::Pattern,
        scrutinee_type: TypeId,
        site: Span,
        ctx: &mut FunctionContext,
    ) -> TirPattern {
        match pattern {
            ast::Pattern::Wildcard => TirPattern::Wildcard,
            ast::Pattern::Ident { id, name, span } => {
                // A bare ident in a pattern is ambiguous: a nullary
                // enum/variant case (`None`, `Red`), an immutable global
                // constant, or a fresh binding. A name annotate bound is a
                // binding; otherwise disambiguate in the same order as
                // `Elaborator::resolve_if_pattern_inner`.
                if self.ann_local_type(*id).is_none() {
                    if let Some(case_index) = self.scrutinee_enum_case_index(scrutinee_type, name) {
                        return TirPattern::Enum {
                            enum_type: self
                                .tysys
                                .type_table
                                .borrow()
                                .scrutinee_structure_head(scrutinee_type),
                            case_name: name.clone(),
                            case_index,
                        };
                    }
                    if self.scrutinee_has_variant_case(scrutinee_type, name) {
                        return self.reify_nullary_variant_case(scrutinee_type, name);
                    }
                    if let Some(constant) = self.reify_pattern_constant(pattern, ctx) {
                        let const_pat = self.constant_value_pattern(constant, scrutinee_type);
                        return self.compare_constant_by_eq(
                            pattern,
                            const_pat,
                            scrutinee_type,
                            ctx,
                        );
                    }
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
                // Mirrors `Elaborator::resolve_if_pattern_inner`'s literal arm:
                // string literals decode their escapes, and `null` on a
                // variant scrutinee with a `None` case lowers to that case.
                let tir_lit = match lit {
                    ast::Literal::String(raw) => {
                        TirLiteralPattern::String(unescape_string(raw).unwrap_or_default())
                    }
                    ast::Literal::Null => {
                        if self.scrutinee_has_variant_case(scrutinee_type, "None") {
                            return self.reify_nullary_variant_case(scrutinee_type, "None");
                        }
                        TirLiteralPattern::Null
                    }
                    // Not pattern literals in the surface grammar; the parser
                    // rejects them, so reaching here is an invariant violation.
                    ast::Literal::Unit
                    | ast::Literal::Bytes(_)
                    | ast::Literal::LocationFile
                    | ast::Literal::LocationLine
                    | ast::Literal::LocationFunction
                    | ast::Literal::DataSection
                    | ast::Literal::IncludeStr(_)
                    | ast::Literal::IncludeBytes(_) => {
                        panic!("literal kind {lit:?} is not valid in pattern position")
                    }
                    ast::Literal::Number(..)
                    | ast::Literal::Byte(_)
                    | ast::Literal::Char(_)
                    | ast::Literal::Bool(_) => {
                        // A literal naming no value is already diagnosed, so
                        // the pattern standing in for it is never lowered.
                        let Ok(value) = literal_pattern(lit) else {
                            return TirPattern::Wildcard;
                        };
                        return self.literal_value_pattern(value, scrutinee_type, site, ctx);
                    }
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
                        let elem_ty = elem_types.get(i).copied().unwrap_or(TypeTable::UNKNOWN);
                        let binding_ty =
                            self.tysys.apply_scrutinee_ref_kind(scrutinee_type, elem_ty);
                        self.reify_pattern(p, binding_ty, site, ctx)
                    })
                    .collect();
                TirPattern::Tuple(sub_patterns, *has_rest)
            }
            ast::Pattern::Variant {
                variant_name,
                bindings,
                ..
            } => {
                // A float type's limit is the one value it holds.
                if let Some(point) = float_limit_pattern(pattern, &self.tysys.resolutions) {
                    return self.literal_value_pattern(point, scrutinee_type, site, ctx);
                }
                // An associated constant (`TokenKind::FOO`, `i32::MAX`) or a
                // namespaced global (`ns::NAME`) matches by value. No case is
                // either, so the lookup distinguishes the two.
                if let Some(constant) = self.reify_pattern_constant(pattern, ctx) {
                    let const_pat = self.constant_value_pattern(constant, scrutinee_type);
                    return self.compare_constant_by_eq(pattern, const_pat, scrutinee_type, ctx);
                }

                // Variant patterns appear in `match Some(x) { Some(v) => …
                // }` etc. The case's payload type lives on
                // `tysys.data.variant_cases`; reify reads it to give
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
                        enum_type: self
                            .tysys
                            .type_table
                            .borrow()
                            .scrutinee_structure_head(scrutinee_type),
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
                let peeled_scrutinee = self
                    .tysys
                    .type_table
                    .borrow()
                    .scrutinee_structure_head(scrutinee_type);
                let (case_index, payload_type) =
                    self.variant_case_index_and_payload(peeled_scrutinee, &case_name);

                // Match ergonomics: a reference scrutinee (`self: &Option<T>`)
                // gives the payload binding the reference kind, so `v` is `&T`
                // and forwards to a `&self` method; `&` downgrades a `&mut`.
                // `payload_type` / `enum_type` stay peeled — only the binding
                // scrutinee carries the reference.
                let binding_scrutinee = self
                    .tysys
                    .apply_scrutinee_ref_kind(scrutinee_type, payload_type);
                let sub_patterns: Vec<TirPattern> = bindings
                    .iter()
                    .map(|p| self.reify_pattern(p, binding_scrutinee, site, ctx))
                    .collect();
                let case_index = resolved_case_index(case_index, &case_name);
                TirPattern::Variant {
                    enum_type: peeled_scrutinee,
                    variant_name: case_name,
                    case_index,
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
                    let first = self.reify_pattern(first_alt, scrutinee_type, site, ctx);
                    let first_bindings = collect_pattern_bindings_with_index(&first);
                    resolved.push(first);

                    for alt in alternatives.iter().skip(1) {
                        let alt_resolved = self.reify_pattern(alt, scrutinee_type, site, ctx);
                        let alt_bindings = collect_pattern_bindings_with_index(&alt_resolved);
                        let mut remapped = alt_resolved;
                        for (first_bind, alt_bind) in first_bindings.iter().zip(alt_bindings.iter())
                        {
                            if first_bind.1 != alt_bind.1 {
                                remap_pattern_local(&mut remapped, alt_bind.1, first_bind.1);
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
                let inclusive = matches!(kind, RangeKind::Inclusive);
                let Some(range) = range_pattern(start, end, inclusive, &self.tysys.resolutions)
                else {
                    let shown = unparse_pattern_source(pattern);
                    return self.reify_constant_range(
                        [start, end],
                        inclusive,
                        shown,
                        scrutinee_type,
                        ctx,
                    );
                };
                let range = range.expect("annotate diagnoses a range bound that names no value");
                self.literal_value_pattern(range, scrutinee_type, site, ctx)
            }
            ast::Pattern::Struct {
                fields, has_rest, ..
            } => self.reify_struct_pattern(fields, *has_rest, scrutinee_type, site, ctx),
            ast::Pattern::Typed {
                id,
                pattern: inner,
                span,
                ..
            } => {
                let target = self
                    .ann_pattern_ascription(*id)
                    .expect("annotate records every type pattern's target");
                let narrows = self
                    .tysys
                    .type_table
                    .borrow()
                    .is_resource_narrowing(scrutinee_type, target);
                if narrows {
                    let shown = unparse_pattern_source(pattern);
                    self.reify_narrowing(inner, target, shown, *span, ctx)
                } else {
                    let binding_ty = self.tysys.ascribed_binding_type(scrutinee_type, target);
                    self.reify_pattern(inner, binding_ty, site, ctx)
                }
            }
            // `build_tir_from_state` skips reify for modules with syntax
            // errors, so reify never walks an `Error` placeholder.
            ast::Pattern::Error(_) => {
                unreachable!("reify does not run on modules with syntax errors")
            }
        }
    }

    /// `name: R` over a supertype of `R`: the value binds at `R`, and the pattern
    /// holds only when the class the handle carries lies in `R`'s classes.
    fn reify_narrowing(
        &mut self,
        inner: &ast::Pattern,
        target: TypeId,
        shown: String,
        span: Span,
        ctx: &mut FunctionContext,
    ) -> TirPattern {
        let (name, local_name, local_index) = match inner {
            ast::Pattern::Ident { id, name, span } | ast::Pattern::MutIdent { id, name, span } => {
                let is_mut = matches!(inner, ast::Pattern::MutIdent { .. });
                let local_index = ctx.add_local_at(name.clone(), target, is_mut, Some(*id), *span);
                (Some(name.clone()), name.clone(), local_index)
            }
            ast::Pattern::Wildcard => {
                let local_name = minted_name("narrowed", ctx.fresh_serial());
                let local_index = ctx.add_local(local_name.clone(), target, false, None);
                (None, local_name, local_index)
            }
            other => panic!("annotate rejects a narrowing over {other:?}"),
        };
        let (low, high) = self
            .tysys
            .type_table
            .borrow()
            .narrowing_classes(target)
            .expect("annotate rejects a narrowing to a resource without classes")
            .handle_bounds();
        let handle = || {
            handle_bits(TirExpr::new(
                TirExprKind::Local {
                    index: local_index,
                    name: local_name.clone(),
                },
                target,
                span,
            ))
        };
        let bound = |bits: u64| {
            TirExpr::new(
                TirExprKind::IntLiteral {
                    value: bits,
                    repr: bits.to_string(),
                },
                TypeTable::U64,
                span,
            )
        };
        let compare = |op, left, right| {
            TirExpr::new(
                TirExprKind::Binary {
                    op,
                    left: Box::new(left),
                    right: Box::new(right),
                },
                TypeTable::BOOL,
                span,
            )
        };
        let test = compare(
            TirBinaryOp::And,
            compare(TirBinaryOp::LtEq, bound(low), handle()),
            compare(TirBinaryOp::Lt, handle(), bound(high)),
        );
        TirPattern::Narrow {
            name,
            local_index,
            type_id: target,
            test: Box::new(test),
            shown,
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
        site: Span,
        ctx: &mut FunctionContext,
    ) -> TirPattern {
        // Determine the struct name: explicit `Type::Pattern` wins;
        // otherwise read from the scrutinee.
        // Destructuring through a reference (`let { x, y } = &p`)
        // presents the scrutinee as `&Point`; peel references so the
        // struct decl resolves (fields inherit the reference kind below).
        let peeled_scrutinee = self
            .tysys
            .type_table
            .borrow()
            .scrutinee_structure_head(scrutinee_type);

        // A field index is a fact about the value being destructured, so the
        // scrutinee's head answers and the pattern's qualifier — which annotate
        // checked against this same head — has no say. An unresolved scrutinee
        // falls back to UNKNOWN-typed sub-patterns, matching annotate.
        let scrutinee_head = match self.tysys.type_table.borrow().get(peeled_scrutinee) {
            ResolvedType::Struct { def, .. } => Some(*def),
            ResolvedType::GenericInstance { .. } => {
                self.tysys.type_def(peeled_scrutinee).map(StructDef::Decl)
            }
            _ => None,
        };
        let field_info: hashmap::IndexMap<String, (u32, TypeId)> = {
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
                    .unwrap_or((0, TypeTable::UNKNOWN));
                // Match ergonomics: a field bound through a `&Point` /
                // `&mut Point` scrutinee is `&field` / `&mut field`.
                let binding_ty = self
                    .tysys
                    .apply_scrutinee_ref_kind(scrutinee_type, field_ty);
                let pattern = self.reify_pattern(&f.pattern, binding_ty, site, ctx);
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

    /// The case `case_name` of `owner` that annotate resolved at `site`, built
    /// as the instance it recorded there, with `args` as its payload.
    fn reify_case_construction(
        &mut self,
        owner: DefId,
        case_name: &str,
        site: AstId,
        args: &[ast::Expr],
        expected_type: Option<TypeId>,
        span: Span,
        ctx: &mut FunctionContext,
    ) -> TirExpr {
        let (variant_type, case_index, declared_payload) = {
            let variant = self
                .type_lookup()
                .variant_cases_of(owner)
                .expect("annotate records a variant as a construction's owner");
            let (index, case) = variant
                .case_named(case_name)
                .expect("annotate resolved the case");
            (
                self.constructed_variant_type(site, variant, expected_type),
                u32::try_from(index).expect("case index fits u32"),
                case.payload,
            )
        };
        let type_args = self
            .tysys
            .type_table
            .borrow()
            .nominal_type_args(variant_type)
            .unwrap_or_default();
        let payload_type = self
            .tysys
            .substitute_type_params(declared_payload, &type_args);
        let payload = args
            .first()
            .map(|arg| Box::new(self.reify_expr(arg, ctx, Some(payload_type))));
        // A newtype prefix (`W::J(1)`) keeps its type, which annotate recorded.
        let expr_type = self
            .ann_expression_types(site)
            .filter(|&t| self.tysys.type_table.borrow().is_newtype(t))
            .unwrap_or(variant_type);
        TirExpr::new(
            TirExprKind::VariantConstruct {
                variant_type,
                case_index,
                case_name: case_name.to_string(),
                payload,
            },
            expr_type,
            span,
        )
    }

    /// `builtin::wrap_in_case`: case `case_index` of `variant_type`, the one
    /// annotate found carrying the payload's type, around `payload`.
    fn wrap_in_case(
        &self,
        variant_type: TypeId,
        case_index: u32,
        payload: TirExpr,
        span: Span,
    ) -> TirExpr {
        let case_name = {
            let tt = self.tysys.type_table.borrow();
            let variant = tt
                .nominal_def(variant_type)
                .and_then(|def| self.type_lookup().variant_cases_of(def))
                .expect("annotate found a variant");
            variant.cases[case_index as usize].name.clone()
        };
        TirExpr::new(
            TirExprKind::VariantConstruct {
                variant_type,
                case_index,
                case_name,
                payload: Some(Box::new(payload)),
            },
            variant_type,
            span,
        )
    }

    /// The variant instance a case construction at `site` builds: the one
    /// annotate recorded, or `expected` where a payload-less case left it open.
    fn constructed_variant_type(
        &self,
        site: AstId,
        variant: &VariantInfo,
        expected: Option<TypeId>,
    ) -> TypeId {
        let recorded = match self.ann_generic_instantiations(site) {
            Some(instantiation) => instantiation.instance_type,
            None => self
                .tysys
                .type_table
                .borrow()
                .type_id_of_decl(variant.defined_at),
        };
        self.tysys
            .resolved_variant_type(recorded, expected)
            .unwrap_or(recorded)
    }

    /// A variant case's discriminant and payload type, substituted with
    /// `variant_type`'s type args. `None` where the type declares no such case.
    fn variant_case_index_and_payload(
        &self,
        variant_type: TypeId,
        case_name: &str,
    ) -> (Option<u32>, TypeId) {
        let type_args = match self.tysys.type_table.borrow().get(variant_type).clone() {
            ResolvedType::GenericInstance { type_args, .. } => type_args,
            _ => Vec::new(),
        };
        let (case_index, payload, type_param_indices): (u32, TypeId, Vec<u32>) = {
            let lookup = self.type_lookup();
            let Some(variant_info) = self
                .tysys
                .type_def(variant_type)
                .and_then(|def| lookup.variant_cases_of(def))
            else {
                return (None, TypeTable::UNKNOWN);
            };
            let Some((case_index, case_data)) = variant_info.case_named(case_name) else {
                return (None, TypeTable::UNKNOWN);
            };
            // Extract the variant decl's type-param indices so the
            // substitution map below is keyed by `index` — matching
            // `TypeTable::substitute_type_params`.
            let indices: Vec<u32> = (0..variant_info.type_param_type_ids.len() as u32).collect();
            let case_index = u32::try_from(case_index).expect("case index fits u32");
            (case_index, case_data.payload, indices)
        };
        if type_args.is_empty() {
            return (Some(case_index), payload);
        }
        // Map TypeParam{index} → concrete `type_args[index]`. Recurse
        // through containers (`Ref`, `BuiltinArray`, `GenericInstance`,
        // `Function`, …) via `TypeTable::substitute_type_params`.
        let substitution: hashmap::IndexMap<u32, TypeId> = type_param_indices
            .iter()
            .zip(type_args.iter())
            .map(|(&idx, &t)| (idx, t))
            .collect();
        let payload = self
            .tysys
            .type_table
            .borrow_mut()
            .substitute_type_params(payload, &substitution);
        (Some(case_index), payload)
    }
}

impl TypeSystem {
    /// The bits of `-value` for an integer literal of `type_id`, wrapped to its
    /// width as the operator wraps: `-(-128 as i8)` is `-128`.
    fn negated_int_literal(&self, value: u64, type_id: TypeId) -> u64 {
        let negated = (value as i64).wrapping_neg().cast_unsigned();
        match self.type_table.borrow().primitive_head(type_id) {
            Some(prim) => truncate_int(negated, prim),
            None => negated,
        }
    }

    /// A type pattern that does not narrow keeps the scrutinee's reference kind
    /// on a target that drops it.
    fn ascribed_binding_type(&self, scrutinee_type: TypeId, target: TypeId) -> TypeId {
        let tt = self.type_table.borrow();
        if tt.type_key(scrutinee_type) == tt.type_key(target) || tt.peel_refs(target) != target {
            return target;
        }
        drop(tt);
        self.apply_scrutinee_ref_kind(scrutinee_type, target)
    }

    /// The type of the `*…` result a dispatched index read produces: the recorded
    /// one when resolved, else the `&Output` the index trait returns, peeled.
    fn index_deref_type(&self, recorded: Option<TypeId>, dispatch: &OperatorDispatch) -> TypeId {
        if !dispatch.needs_deref {
            return dispatch.return_type;
        }
        recorded
            .filter(|t| !matches!(self.type_table.borrow().get(*t), ResolvedType::Unknown))
            .unwrap_or_else(|| self.through_ref(dispatch.return_type))
    }

    /// The trait read `*recv.index(idx)` (or `recv.index_value(idx)`) `dispatch`
    /// records, on a receiver already adjusted to its `self_kind`; `deref_type`
    /// types the `*…` when `dispatch.needs_deref`.
    fn build_index_read_from_dispatch(
        &self,
        adjusted: TirExpr,
        idx: TirExpr,
        dispatch: OperatorDispatch,
        deref_type: TypeId,
        span: Span,
    ) -> TirExpr {
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

    fn is_source_place(&self, expr: &TirExpr) -> bool {
        is_source_place(expr, self.type_table.borrow().compiler_items())
    }

    /// The format of `ty` when it is a half, whose value is carried as bits.
    fn half_format(&self, ty: TypeId) -> Option<FloatFormat> {
        self.type_table
            .borrow()
            .primitive_head(ty)
            .filter(|p| p.is_half())
            .and_then(FloatFormat::of)
    }

    /// The expected type standing in for an unresolved generic variant type,
    /// when it names the same variant with its type parameters filled.
    fn resolved_variant_type(&self, recorded: TypeId, expected: Option<TypeId>) -> Option<TypeId> {
        let expected = expected?;
        let table = self.type_table.borrow();
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

    /// `inner` wrapped in the reference kind of `scrutinee_type` for match
    /// ergonomics, as [`Self::peel_scrutinee_refs`] reckons it.
    fn apply_scrutinee_ref_kind(&self, scrutinee_type: TypeId, inner: TypeId) -> TypeId {
        match self.peel_scrutinee_refs(scrutinee_type, RefBinding::None).1 {
            RefBinding::None => inner,
            RefBinding::Ref => self.type_table.borrow_mut().make_ref(inner),
            RefBinding::MutRef => self.type_table.borrow_mut().make_mut_ref(inner),
        }
    }
}

/// Attribute extractors, reify's own. An attribute is uniquely determined by
/// the AST alone, which is what the completeness rule lets reify re-derive, so
/// these need no recorded fact and no elaborator to run.
fn extract_is_ambient_attr(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|a| a.name == AMBIENT)
}

fn extract_inline_hint_attr(attrs: &[Attribute]) -> tir::InlineHint {
    let Some(attr) = attrs.iter().find(|a| a.name == INLINE) else {
        return tir::InlineHint::Auto;
    };
    match attr.args.first().map(AttrArg::as_str) {
        Some("always") => tir::InlineHint::Always,
        Some("never") => tir::InlineHint::Never,
        None => tir::InlineHint::Hint,
        _ => tir::InlineHint::Auto,
    }
}

fn extract_export_name_attr(attrs: &[Attribute]) -> Option<String> {
    attrs
        .iter()
        .find(|a| a.name == EXPORT_NAME)
        .and_then(|a| a.args.first())
        .map(|a| a.as_str().to_string())
}

fn extract_allocator_tag_attr(attrs: &[Attribute]) -> Option<String> {
    attrs
        .iter()
        .find(|a| a.name == ALLOC)
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

/// The receiver node the recorded `(self_kind, is_ref_impl)` pair asks for,
/// typed as the body walk's own answer
/// ([`super::method_lookup::adjusted_receiver_type`]): the layers that answer
/// keeps read through, and the reference it adds taken.
fn adjust_receiver_for_self_kind(
    receiver: TirExpr,
    self_kind: ast::SelfKind,
    is_ref_impl: bool,
    type_table: &std::cell::RefCell<TypeTable>,
) -> TirExpr {
    let target = adjusted_receiver_type(receiver.type_id, self_kind, is_ref_impl, type_table);
    let tt = type_table.borrow();
    let held = tt.ref_depth(target);
    if tt.type_key(tt.ref_layers(receiver.type_id, held)) == tt.type_key(target) {
        return deref_to_layers(receiver, held, &tt);
    }
    to_layers_of(deref_to_layers(receiver, held - 1, &tt), target, &tt)
}

/// The TIR read of a resolved binding: a local, a by-value capture, or a
/// dereference of a captured `&mut`.
fn var_ref_expr(var_ref: VarRef, name: &str, span: Span) -> TirExpr {
    match var_ref {
        VarRef::Local { index, type_id, .. } => TirExpr::new(
            TirExprKind::Local {
                index,
                name: name.to_string(),
            },
            type_id,
            span,
        ),
        VarRef::Capture { index, type_id, .. } => TirExpr::new(
            TirExprKind::Capture {
                index,
                name: name.to_string(),
            },
            type_id,
            span,
        ),
        VarRef::DerefCapture {
            index,
            ref_type_id,
            inner_type_id,
            ..
        } => {
            let capture = TirExpr::new(
                TirExprKind::Capture {
                    index,
                    name: deref_capture_name(index),
                },
                ref_type_id,
                span,
            );
            TirExpr::new(
                TirExprKind::Unary {
                    op: TirUnaryOp::Deref,
                    expr: Box::new(capture),
                },
                inner_type_id,
                span,
            )
        }
    }
}

/// `==` / `!=` on unrestricted resource handles: bit equality, since the host
/// interns them.
fn handle_comparison(op: ast::BinaryOp, left: TirExpr, right: TirExpr, span: Span) -> TirExpr {
    let op = if op == ast::BinaryOp::Eq {
        TirBinaryOp::Eq
    } else {
        TirBinaryOp::NotEq
    };
    TirExpr::new(
        TirExprKind::Binary {
            op,
            left: Box::new(handle_bits(left)),
            right: Box::new(handle_bits(right)),
        },
        TypeTable::BOOL,
        span,
    )
}

/// Wrap an `Ord::cmp` call into a `bool` by comparing the returned
/// `Ordering` variant against the one that makes the operator true:
/// `<` → `cmp == Less`, `>` → `cmp == Greater`,
/// `<=` → `cmp != Greater`, `>=` → `cmp != Less`.
pub(crate) fn ord_bool_from_cmp(
    cmp_call: TirExpr,
    op: ast::BinaryOp,
    span: Span,
    type_table: &std::cell::RefCell<TypeTable>,
) -> TirExpr {
    let ordering_type_id = type_table
        .borrow_mut()
        .make_compiler_enum(CompilerItem::Ordering);
    // Look up Ordering's `Less` / `Greater` cases through the
    // `CompilerItem` registry so a stdlib rename of either case
    // flows here without touching the operator-lowering path.
    let (less_name, less_index, greater_name, greater_index) = {
        let tt = type_table.borrow();
        let items = tt.compiler_items();
        let (_, _, less_name, less_index) = items.require_enum_case(CompilerItem::OrderingLess);
        let (_, _, greater_name, greater_index) =
            items.require_enum_case(CompilerItem::OrderingGreater);
        (
            less_name.to_string(),
            less_index,
            greater_name.to_string(),
            greater_index,
        )
    };
    let (compare_op, case_name, case_index): (TirBinaryOp, String, u32) = match op {
        ast::BinaryOp::Lt => (TirBinaryOp::Eq, less_name, less_index),
        ast::BinaryOp::Gt => (TirBinaryOp::Eq, greater_name, greater_index),
        ast::BinaryOp::LtEq => (TirBinaryOp::NotEq, greater_name, greater_index),
        ast::BinaryOp::GtEq => (TirBinaryOp::NotEq, less_name, less_index),
        _ => unreachable!(),
    };
    let ordering_variant = TirExpr::new(
        TirExprKind::EnumConstruct {
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
        TirExprKind::Binary {
            op: compare_op,
            left: Box::new(cmp_call),
            right: Box::new(ordering_variant),
        },
        TypeTable::BOOL,
        span,
    )
}

/// The sole constructor of a method call — a [`crate::tir::TirExprKind::Call`]
/// whose receiver heads its `args`. Centralising it gives one audit point for
/// "every emitted method call was typechecked against the callee's declared
/// parameter types", though the typechecking itself is annotate's job.
fn build_tir_method_call(
    receiver: TirExpr,
    func: tir::FunctionRef,
    type_args: Vec<TypeId>,
    args: Vec<CallArg>,
    return_type: TypeId,
    span: Span,
) -> TirExpr {
    TirExpr::new(
        TirExprKind::method_call(Box::new(receiver), func, type_args, args),
        return_type,
        span,
    )
}

/// Which of `params` a default the call leaves out names, by slot, read off the
/// spelling: a name the default rebinds for itself answers too, at a `let`'s cost.
fn slots_named_by_defaults(args_len: usize, params: &[(String, Option<ast::Expr>)]) -> Vec<bool> {
    let mut named = vec![false; params.len()];
    for (i, (_, default)) in params.iter().enumerate().skip(args_len) {
        let Some(default) = default else {
            break;
        };
        let mut spelled = SpelledNames::default();
        spelled.visit_expr(default);
        for (slot, (name, _)) in params[..i].iter().enumerate() {
            named[slot] |= name != RECEIVER && spelled.0.contains(name);
        }
    }
    named
}

/// The names a closure spells in any default the call leaves to its declaration.
fn names_captured_by_defaults(
    args_len: usize,
    params: &[(String, Option<ast::Expr>)],
) -> IndexSet<String> {
    let mut captured = ClosureSpelledNames::default();
    for (_, default) in params.iter().skip(args_len) {
        let Some(default) = default else {
            break;
        };
        captured.visit_expr(default);
    }
    captured.0.0
}

/// Every name an expression spells as an identifier.
#[derive(Default)]
struct SpelledNames(IndexSet<String>);

impl AstVisitor<'_> for SpelledNames {
    fn visit_expr(&mut self, expr: &ast::Expr) {
        if let ast::Expr::Ident(ident) = expr {
            self.0.insert(ident.name.clone());
        }
        walk_expr(self, expr);
    }
}

/// Every name an expression spells inside a closure.
#[derive(Default)]
struct ClosureSpelledNames(SpelledNames);

impl AstVisitor<'_> for ClosureSpelledNames {
    fn visit_expr(&mut self, expr: &ast::Expr) {
        if let ast::Expr::Closure(_) = expr {
            self.0.visit_expr(expr);
            return;
        }
        walk_expr(self, expr);
    }
}

fn is_mut_ref(expr: &TirExpr) -> bool {
    matches!(
        expr.kind,
        TirExprKind::Unary {
            op: TirUnaryOp::MutRef,
            ..
        }
    )
}

/// The `&mut`s a call keeps in place, each with the local a default reads it
/// through; [`TirMutVisitor::visit_expr`] reads the place again there.
#[derive(Default)]
struct MutRefPlaces(Vec<(u32, TirExpr)>);

impl TirMutVisitor for MutRefPlaces {
    fn visit_expr(&mut self, expr: &mut TirExpr) {
        if let TirExprKind::Local { index, .. } = &expr.kind
            && let Some((_, mut_ref)) = self.0.iter().find(|(local, _)| local == index)
        {
            let span = expr.span;
            *expr = mut_ref.clone();
            expr.span = span;
            return;
        }
        // A closure body numbers its own locals, and none captures a stand-in:
        // `names_captured_by_defaults` keeps those `&mut`s bound.
        if matches!(expr.kind, TirExprKind::Closure { .. }) {
            return;
        }
        self.walk_expr(expr);
    }
}

/// Bind an argument or default that a later default names, to a minted local
/// the default reaches under the parameter's own name.
fn bind_param_to_local(
    ctx: &mut FunctionContext,
    param: &str,
    value: TirExpr,
    stmts: &mut Vec<TirStmt>,
) -> TirExpr {
    let name = minted_name(param, ctx.fresh_serial());
    let read = bind_to_local(ctx, name, value, stmts);
    let TirExprKind::Local { index, .. } = &read.kind else {
        unreachable!("`bind_to_local` answers the read of the local it binds")
    };
    ctx.name_local(param.to_string(), *index);
    read
}

/// Bind `value` to a new local `name` (the `let` pushed onto `stmts`), and
/// answer the read of it.
fn bind_to_local(
    ctx: &mut FunctionContext,
    name: String,
    value: TirExpr,
    stmts: &mut Vec<TirStmt>,
) -> TirExpr {
    let (type_id, span) = (value.type_id, value.span);
    let index = ctx.add_local(name.clone(), type_id, false, None);
    stmts.push(TirStmt::new(
        TirStmtKind::Let {
            name: name.clone(),
            local_index: index,
            value,
            is_mut: false,
            type_id,
            storage: LetStorage::Planned,
        },
        span,
    ));
    TirExpr::new(TirExprKind::Local { index, name }, type_id, span)
}

/// `if $contract_clause() { check }`: one `contract` clause, marked for
/// `lower::contract_checks` to copy to each call.
fn contract_check(check: Vec<TirStmt>, span: Span) -> TirStmt {
    TirStmt::new(
        TirStmtKind::If {
            condition: builtin_call(CONTRACT_CLAUSE_INTRINSIC, Vec::new(), TypeTable::BOOL),
            then_block: TirBlock::new(check, span),
            else_block: None,
        },
        span,
    )
}

/// Retarget the call `unchecked` marks, spanning `call_span`, to its callee's
/// twin, which the check of `unchecked`'s operand established it has. A
/// marked subscript goes through [`unchecked_subscript`] instead.
/// Reify may wrap that call (a default argument naming another parameter
/// hoists the arguments into a block around it), so the call is found by the
/// span of the operand it was reified from.
fn retarget_to_unchecked_twin(expr: &mut TirExpr, call_span: Span) {
    struct Marked {
        call_span: Span,
        found: bool,
    }
    impl TirMutVisitor for Marked {
        fn visit_expr(&mut self, expr: &mut TirExpr) {
            if self.found {
                return;
            }
            match &mut expr.kind {
                TirExprKind::Call { func, .. } if expr.span == self.call_span => {
                    func.retarget_to_unchecked_twin();
                    self.found = true;
                }
                _ => self.walk_expr(expr),
            }
        }
    }
    let mut marked = Marked {
        call_span,
        found: false,
    };
    marked.visit_expr(expr);
    assert!(marked.found, "`unchecked` marks a call: {:?}", expr.kind);
}

/// `dispatch`, aimed at its callee's `unchecked` twin where `index` is marked,
/// which the check of the mark established it has.
fn unchecked_subscript(index: &ast::IndexExpr, mut dispatch: OperatorDispatch) -> OperatorDispatch {
    if index.unchecked.is_some() {
        dispatch.function_ref.retarget_to_unchecked_twin();
    }
    dispatch
}

/// The `unchecked` twin of `func`, reified from `decl`, where `decl` writes a
/// `contract` clause. See [`split_unchecked_twin`].
fn unchecked_twin(
    decl: &ast::Function,
    func: &mut TirFunction,
    module: &ModuleSource,
    type_table: &mut TypeTable,
) -> Option<TirFunction> {
    let clauses = decl.contracts().count();
    (clauses > 0).then(|| split_unchecked_twin(func, clauses, module, type_table))
}

/// Split `func`, whose body opens with its `clauses` `contract` clauses as
/// [`contract_check`]s, into its `unchecked` twin, holding the rest of the
/// body, and `func` itself, left as the clauses and a call to the twin.
fn split_unchecked_twin(
    func: &mut TirFunction,
    clauses: usize,
    module: &ModuleSource,
    type_table: &mut TypeTable,
) -> TirFunction {
    // Linking stamps every function's module, but the call to the twin
    // records it now: a call to an instance resolves by name in that module.
    func.module_source = module.clone();
    let body = func
        .body
        .as_mut()
        .expect("a function with a `contract` clause has a body");
    // What runs ahead of the clauses, a coverage probe, counts the body run.
    let mut rest = std::mem::take(&mut body.stmts);
    let first = rest
        .iter()
        .position(is_contract_clause)
        .expect("a function with a `contract` clause reifies to a contract check");
    let checks: Vec<TirStmt> = rest.drain(first..first + clauses).collect();
    assert!(
        checks.iter().all(is_contract_clause) && !rest.iter().any(is_contract_clause),
        "every `contract` clause of `{}` reifies to a contract check, one run of them in its body",
        func.name
    );
    body.stmts = checks;
    let mut twin = func.clone();
    twin.body = Some(TirBlock::new(rest, func.span));
    twin.name = unchecked_twin_name(&func.name);
    twin.def_id = None;
    twin.kind = FunctionKind::UncheckedTwin {
        of: func
            .def_id
            .expect("a function with a `contract` clause is declared"),
    };
    twin.is_export = false;
    twin.export_name = None;
    twin.compiler_item = None;
    if let Some(info) = &mut twin.method_info {
        info.mark_unchecked();
    }
    let call = call_with_params(func, &twin, type_table);
    let forward = if func.return_type == TypeTable::UNIT {
        TirStmtKind::Expr(call)
    } else {
        TirStmtKind::Return { value: Some(call) }
    };
    func.body
        .as_mut()
        .expect("split above")
        .stmts
        .push(TirStmt::new(forward, func.span));
    twin
}

/// Bind an impl's `methods` to the `contract` clauses their trait declares.
/// The method a clause holder holds the clauses of splits as one writing its
/// own clauses does, with a call to the holder as its one clause.
/// `bindings` pairs each holder's declaration with the method's, as
/// [`Reify::contract_bindings`] answers them.
fn bind_trait_contracts(
    mut methods: Vec<TirFunction>,
    bindings: &[(DefId, DefId)],
    module: &ModuleSource,
    type_table: &mut TypeTable,
) -> Vec<TirFunction> {
    let position =
        |methods: &[TirFunction], def: DefId| methods.iter().position(|m| m.def_id == Some(def));
    for &(holder_def, held_def) in bindings {
        let Some(holder_at) = position(&methods, holder_def) else {
            continue;
        };
        methods[holder_at].module_source = module.clone();
        let holder = methods[holder_at].clone();
        if let Some(held_at) = position(&methods, held_def) {
            let method = &mut methods[held_at];
            // An impl writing clauses of its own under the trait's is an error
            // annotate reported, and its own split stands.
            if method
                .body
                .iter()
                .flat_map(|body| &body.stmts)
                .any(is_contract_clause)
            {
                continue;
            }
            let open = call_with_params(method, &holder, type_table);
            method
                .body
                .as_mut()
                .expect("a method a trait's contract binds has a body")
                .stmts
                .insert(
                    0,
                    contract_check(
                        vec![TirStmt::new(TirStmtKind::Expr(open), holder.span)],
                        holder.span,
                    ),
                );
            let twin = split_unchecked_twin(method, 1, module, type_table);
            methods.push(twin);
        }
    }
    methods
}

/// A call to `callee`, passing it `func`'s parameters and type parameters,
/// which `callee` declares as well. A call passes no effect argument.
fn call_with_params(
    func: &TirFunction,
    callee: &TirFunction,
    type_table: &mut TypeTable,
) -> TirExpr {
    let span = callee.span;
    // A method's own parameters are numbered after its impl's.
    let offset = func.impl_type_params.len() as u32;
    let type_args: Vec<TypeId> = func
        .type_params
        .iter()
        .filter(|param| !param.is_effect)
        .map(|param| {
            type_table.make_declared_param(param.name.clone(), offset + param.index, param.is_pack)
        })
        .collect();
    assert_eq!(func.type_params.len(), callee.type_params.len());
    assert_eq!(func.params.len(), callee.params.len());
    let args = func
        .params
        .iter()
        .zip(&callee.params)
        .map(|(param, callee_param)| {
            let local = TirExprKind::Local {
                index: param.local_index,
                name: param.name.clone(),
            };
            CallArg::new(
                TirExpr::new(local, param.type_id, span),
                callee_param.is_mut,
            )
        })
        .collect();
    TirExpr::new(
        TirExprKind::Call {
            func: Box::new(FunctionRef::from_resolved(
                callee,
                callee.module_source.clone(),
            )),
            type_args,
            args: CallArgs::free(args),
        },
        callee.return_type,
        span,
    )
}

fn ast_unary_op_to_tir(op: ast::UnaryOp) -> TirUnaryOp {
    match op {
        ast::UnaryOp::Neg => TirUnaryOp::Neg,
        ast::UnaryOp::Not => TirUnaryOp::Not,
        ast::UnaryOp::BitNot => TirUnaryOp::BitNot,
        ast::UnaryOp::Ref => TirUnaryOp::Ref,
        ast::UnaryOp::MutRef => TirUnaryOp::MutRef,
        ast::UnaryOp::Deref => TirUnaryOp::Deref,
        ast::UnaryOp::Unchecked => panic!("`unchecked` is reified as its operand"),
    }
}

/// Map an AST [`ast::BinaryOp`] to its TIR counterpart. The mapping is
/// 1:1 for the source-level ops; TIR adds `RefNotEq` as an internal
/// variant that lowering synthesises, so this helper never produces it.
fn ast_binary_op_to_tir(op: ast::BinaryOp) -> TirBinaryOp {
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
    wire_value_of(attrs, "name")
}

/// The string a `#[wire(key = "...")]` on the declaration writes.
fn wire_value_of(attrs: &[ast::Attribute], key: &str) -> Option<String> {
    attrs.iter().find_map(|a| {
        if a.name == WIRE {
            a.kv_value(key).map(str::to_string)
        } else {
            None
        }
    })
}
/// `#[wire(name_policy = "...")]` on a declaration. The attribute check refuses
/// a policy `NamePolicy` does not name.
fn wire_name_policy_of(attrs: &[ast::Attribute]) -> Option<NamePolicy> {
    attrs.iter().find_map(|a| {
        if a.name == WIRE {
            a.kv_value("name_policy").and_then(NamePolicy::parse)
        } else {
            None
        }
    })
}

/// Whether the declaration reserves a name rather than a signature. Nothing
/// ever calls a `#[unavailable]` one, so it states nothing about a call and is
/// owed nothing about one either.
fn reserves_a_name_only(func: &ast::Function) -> bool {
    func.unavailable_attr().is_some()
}

/// Whether the declaration carries `#[cm(...)]`, which makes it a Component
/// Model import called through an adapter. Not `Function::is_cm_import`, which
/// a `#[canonical(...)]` builtin answers too.
fn carries_cm(func: &ast::Function) -> bool {
    func.attrs.iter().any(|attr| attr.name == CM)
}

/// The code a fault in the attribute named `name` is reported under.
fn code_of(name: &str) -> Code {
    if name == STORAGE {
        Code::StorageAttr
    } else {
        assert_eq!(name, SIDE_EFFECT, "a fact attribute is one of the two");
        Code::SideEffectAttr
    }
}

/// The discriminant of a case the walk resolved. The walk rejects a case the
/// variant does not declare, so reify only ever sees one it resolved.
fn resolved_case_index(case_index: Option<u32>, case_name: &str) -> u32 {
    case_index.unwrap_or_else(|| {
        unreachable!("reify does not run on a pattern naming no case: `{case_name}`")
    })
}

/// First `return <value>` type reachable in a reified closure body.
///
/// TIR counterpart of `control_flow::find_return_type_in_block`, and must stay
/// in step with it: a construct missing here is a `return` the closure's return
/// type cannot see, which mistypes `$call` and fails core-Wasm validation.
fn tir_block_return_type(body: &TirExpr) -> Option<tir::TypeId> {
    fn in_block(block: &TirBlock) -> Option<tir::TypeId> {
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

    fn in_expr(expr: &TirExpr) -> Option<tir::TypeId> {
        match &expr.kind {
            TirExprKind::Block(block) => in_block(block),
            TirExprKind::If {
                then_branch,
                else_branch,
                ..
            } => in_block(then_branch).or_else(|| else_branch.as_ref().and_then(in_block)),
            TirExprKind::Match { arms, .. } => arms.iter().find_map(|arm| in_expr(&arm.body)),
            TirExprKind::WithHandler { body, .. } => in_block(body),
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
pub(crate) fn default_impl_methods(decl: &InterfaceDecl) -> Vec<ast::Function> {
    decl.methods
        .iter()
        .filter(|method| method.body.is_some())
        .map(|method| ast::Function {
            name: effect_default_impl_name(&decl.name, &method.name),
            ..method.clone()
        })
        .collect()
}

/// A call of the `core:rt` function `item` on `arg`.
pub(super) fn rt_call(
    type_table: &TypeTable,
    item: CompilerItem,
    arg: TirExpr,
    result_type: TypeId,
    span: Span,
) -> TirExpr {
    let (module_source, name) = type_table.compiler_items().require_function(item);
    let func = FunctionRef {
        module_source: module_source.clone(),
        name: name.to_string(),
        template: None,
        monomorph_info: None,
        method_info: None,
    };
    unary_call(func, arg, result_type, span)
}

/// The exact `f32` that `value`, of the half type `half`, holds.
fn widen_half(type_table: &TypeTable, value: TirExpr, half: PrimitiveType, span: Span) -> TirExpr {
    let (base, widen) = match half {
        PrimitiveType::F16 => (TypeTable::F16, CompilerItem::F16Widen),
        PrimitiveType::Bf16 => (TypeTable::BF16, CompilerItem::Bf16Widen),
        _ => unreachable!("`{}` is no half", half.as_str()),
    };
    rt_call(
        type_table,
        widen,
        bare_cast(value, base, span),
        TypeTable::F32,
        span,
    )
}

/// `pattern` on the settled `scrutinee`, or why it names no value of it.
pub(crate) fn lower_instance_pattern(
    pattern: &InstancePattern,
    scrutinee: TypeId,
    type_table: &mut TypeTable,
    add_local: impl FnMut(String, TypeId) -> u32,
    span: Span,
) -> Result<TirPattern, Vec<PatternLiteralError>> {
    Ok(
        match settle_instance_pattern(pattern, scrutinee, type_table)? {
            SettledPattern::Literal(value) => TirPattern::Literal(value),
            SettledPattern::Range {
                start,
                end,
                inclusive,
                is_unsigned,
            } => TirPattern::Range {
                start,
                end,
                inclusive,
                is_unsigned,
            },
            SettledPattern::Float(range) => {
                let locals = reserve_float_pattern_locals(scrutinee, type_table, add_local);
                let shown = unparse_instance_pattern(pattern);
                float_range_narrow(&range, &locals, shown, type_table, span)
            }
        },
    )
}

/// The locals a float pattern compares its scrutinee through: the scrutinee,
/// and for a half the `f32` it widens to, read by every comparison so the
/// widening runs once.
pub(super) struct FloatPatternLocals {
    float_type: TypeId,
    scrutinee: u32,
    widened: Option<u32>,
}

/// Reserve the locals of a float pattern on `scrutinee`, in the order annotate
/// and reify both take them.
pub(super) fn reserve_float_pattern_locals(
    scrutinee: TypeId,
    type_table: &TypeTable,
    mut add_local: impl FnMut(String, TypeId) -> u32,
) -> FloatPatternLocals {
    // Lowering peels a reference scrutinee, as match ergonomics reads it.
    let float_type = type_table.peel_refs(scrutinee);
    let scrutinee = add_local(range_local_name(), float_type);
    let is_half = matches!(
        type_table.primitive_head(float_type),
        Some(PrimitiveType::F16 | PrimitiveType::Bf16)
    );
    let widened = is_half.then(|| add_local(widened_half_local_name(), TypeTable::F32));
    FloatPatternLocals {
        float_type,
        scrutinee,
        widened,
    }
}

/// A float range holding its scrutinee in `locals`, matching where it lies
/// between the bounds: a NaN where the range is the NaN, the value where it is
/// one. No bound of a wider range is a NaN, so IEEE's comparisons answer as the
/// float order does, one instruction each. A half compares as the `f32` it
/// widens to exactly.
fn float_range_narrow(
    range: &FloatRange,
    locals: &FloatPatternLocals,
    shown: String,
    type_table: &TypeTable,
    span: Span,
) -> TirPattern {
    let float_type = locals.float_type;
    let scrutinee = TirExpr::new(
        TirExprKind::Local {
            index: locals.scrutinee,
            name: range_local_name(),
        },
        float_type,
        span,
    );
    let (value, operand, width, widen) =
        match (type_table.primitive_head(float_type), locals.widened) {
            (Some(half @ (PrimitiveType::F16 | PrimitiveType::Bf16)), Some(widened)) => {
                let name = widened_half_local_name();
                let widen = TirStmt::new(
                    TirStmtKind::Let {
                        name: name.clone(),
                        local_index: widened,
                        is_mut: false,
                        type_id: TypeTable::F32,
                        value: widen_half(type_table, scrutinee, half, span),
                        storage: LetStorage::Planned,
                    },
                    span,
                );
                let value = TirExpr::new(
                    TirExprKind::Local {
                        index: widened,
                        name,
                    },
                    TypeTable::F32,
                    span,
                );
                (value, TypeTable::F32, "f32", Some(widen))
            }
            (Some(PrimitiveType::F32), None) => (
                bare_cast(scrutinee, TypeTable::F32, span),
                TypeTable::F32,
                "f32",
                None,
            ),
            (Some(PrimitiveType::F64), None) => (
                bare_cast(scrutinee, TypeTable::F64, span),
                TypeTable::F64,
                "f64",
                None,
            ),
            _ => unreachable!("a float pattern reads a float, and a half alone widens"),
        };
    let bound = |bits: u64| {
        let value = range.format.value(bits);
        TirExpr::new(
            TirExprKind::FloatLiteral {
                value,
                repr: format_float_repr(value),
            },
            operand,
            span,
        )
    };
    let compare = |op: &str, left, right| {
        builtin_call(
            &format!("{width}_ieee754_{op}"),
            vec![left, right],
            TypeTable::BOOL,
        )
    };
    let test = if range.is_nan() {
        compare("ne", value.clone(), value)
    } else if range.is_point() {
        compare("eq", value, bound(range.start))
    } else {
        let below_end = if range.inclusive { "le" } else { "lt" };
        TirExpr::new(
            TirExprKind::Binary {
                op: TirBinaryOp::And,
                left: Box::new(compare("ge", value.clone(), bound(range.start))),
                right: Box::new(compare(below_end, value, bound(range.end))),
            },
            TypeTable::BOOL,
            span,
        )
    };
    let test = match widen {
        Some(widen) => {
            let stmts = vec![widen, TirStmt::new(TirStmtKind::Expr(test), span)];
            TirExpr::new(
                TirExprKind::Block(TirBlock::new(stmts, span)),
                TypeTable::BOOL,
                span,
            )
        }
        None => test,
    };
    TirPattern::Narrow {
        name: None,
        local_index: locals.scrutinee,
        type_id: float_type,
        test: Box::new(test),
        shown,
    }
}

/// The literal of type `ty` holding a range bound's `value`.
fn bound_literal(value: BoundValue, ty: TypeId, type_table: &TypeTable, span: Span) -> TirExpr {
    let kind = match value {
        BoundValue::Int(bits) => TirExprKind::IntLiteral {
            value: bits as u64,
            repr: if type_table.is_unsigned_int(ty) {
                bits.cast_unsigned().to_string()
            } else {
                bits.to_string()
            },
        },
        BoundValue::Char(c) => TirExprKind::CharLiteral(c),
        BoundValue::Float(_, bits) if type_table.is_half(ty) => {
            return half_literal(bits, ty, span);
        }
        BoundValue::Float(format, bits) => {
            let value = format.value(bits);
            TirExprKind::FloatLiteral {
                value,
                repr: format_float_repr(value),
            }
        }
    };
    TirExpr::new(kind, ty, span)
}

/// A call of `func` on the one argument `arg`.
fn unary_call(func: FunctionRef, arg: TirExpr, result_type: TypeId, span: Span) -> TirExpr {
    TirExpr::new(
        TirExprKind::Call {
            func: Box::new(func),
            type_args: vec![],
            args: CallArgs::free(vec![CallArg::new(arg, false)]),
        },
        result_type,
        span,
    )
}

/// `expr as to` as codegen takes it, one instruction or none: `expr` itself
/// where it already has the type.
fn bare_cast(expr: TirExpr, to: TypeId, span: Span) -> TirExpr {
    if expr.type_id == to {
        return expr;
    }
    TirExpr::new(
        TirExprKind::Cast {
            expr: Box::new(expr),
            target_type: to,
        },
        to,
        span,
    )
}

/// A half precision literal, given as its bits.
fn half_literal(bits: u64, ty: TypeId, span: Span) -> TirExpr {
    TirExpr::new(
        TirExprKind::IntLiteral {
            value: bits,
            repr: format!("{bits:#06x}"),
        },
        ty,
        span,
    )
}
