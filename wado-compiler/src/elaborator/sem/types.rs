//! [`TypeAnnotations`] — per-[`crate::ast::AstId`] type annotations and
//! dispatch decisions recorded during the body walk.
//!
//! # Membership rule
//!
//! Add a field here when it stores a fact keyed by an [`crate::ast::AstId`]
//! (or [`crate::symbol::SymbolKey`]) produced as a *decision* by the
//! body-level elaborator: the resolved type of an expression, the chosen
//! method dispatch target, the chosen coercion, the desugar kind of a
//! TIR-direct rewrite. This is what [`super::super::reify`] (Stage 5) will
//! read in lieu of re-running inference.
//!
//! Facts that derive purely from the AST (spans, position lookup) belong
//! on [`crate::ast_index::AstIndex`], not here. Decl-level facts (function
//! return types, generic-parameter tables) belong on
//! [`super::decls::ModuleDecls`].
//!
//! Stage 3 of [`wep-2026-05-26-elaborator-rearchitecture.md`] populated
//! `local_types`; Stage 4 adds `expression_types` (per-`AstId` resolved
//! type for every expression visited by the body walk),
//! `method_dispatch` (per-`MethodCallExpr` resolved target plus the
//! receiver-adjustment kind that annotate picked), `coercions`
//! (per-`AstId` coercion choice that
//! [`super::super::Elaborator::try_coerce`] applied), and `desugars`
//! (per-`AstId` rewrite tag for the TIR-direct desugar sites: `assert`,
//! `matches`, comparison chains, `for x of …`, `while`, and compound
//! assignment).
//!
//! Stage 5 extends the map set with the remaining decisions reify needs:
//! `generic_instantiations` (call/struct/variant generic type args plus
//! the resulting instance `TypeId`), `closure_captures` (the capture
//! list plus the mut-capture `__ref_*` materialisation order),
//! `assert_captures` (the power-assert capture-slot table), and
//! `for_of_iterator` (the resolved `into_iter`/`next` dispatch targets
//! for the iterator path of `for x of expr`). `MethodDispatch` also
//! grows an `is_ref_impl` flag that reify needs alongside `self_kind`
//! to drive `adjust_receiver_for_self_kind`. See
//! `wep-2026-05-26-elaborator-rearchitecture.md` §`Design notes (Stage 5)`
//! for the full gap inventory.

use crate::ast::{self, AstId};
use crate::hashmap::IndexMap;
use crate::symbol::SymbolKey;
use crate::tir::{FunctionRef, TypeId};

/// Method-dispatch decision recorded by the body walk for a
/// [`crate::ast::MethodCallExpr`].
///
/// `function_ref` captures the resolved target (module, mangled name,
/// monomorph info, and method-name metadata) so the future `reify` pass
/// can emit the [`crate::tir::TirExprKind::MethodCall`] without re-running
/// trait lookup, blanket-impl selection, or method-name mangling.
/// `self_kind` carries the receiver-adjustment decision (`self` / `&self`
/// / `&mut self`) so reify can drive `adjust_receiver_for_self_kind` with
/// the same kind that annotate used.
///
/// Short-circuiting paths inside
/// [`super::super::Elaborator::resolve_method_call_with`] (tuple `.len()`
/// / `.zip()`, the static-method-as-instance error) do *not* leave an
/// entry here — they rewrite the call into a non-`MethodCall` TIR shape
/// that reify recognises from the receiver type alone. The synthetic
/// for-of `.into_iter()` / `.next()` dispatches also skip recording
/// because they have no source-level `MethodCallExpr` to attach to.
///
/// `#[allow(dead_code)]` on the fields is intentional: nothing reads them
/// yet because the consumer (`reify`, Stage 5 of the WEP) has not landed.
/// The Stage 4 contract is "the data is recorded and reachable;" the read
/// path arrives with reify.
///
/// Stage 5 adds [`Self::is_ref_impl`]: receiver adjustment for `&self` /
/// `&mut self` requires not only `self_kind` but also whether the method
/// was found on a reference-type impl (`impl Trait for &T`), because such
/// impls take an *extra* layer of `&` on the receiver. The flag is the
/// output of `lookup_method_info` and the input reify feeds to
/// `adjust_receiver_for_self_kind` (Gap 2 in
/// `wep-2026-05-26-elaborator-rearchitecture.md` §`Design notes (Stage 5)`).
#[derive(Clone)]
pub(crate) struct MethodDispatch {
    #[allow(dead_code)]
    pub(crate) function_ref: FunctionRef,
    #[allow(dead_code)]
    pub(crate) self_kind: ast::SelfKind,
    /// True when the resolved method's impl was found on a reference type
    /// (`impl Trait for &T`). Reify wraps the receiver in an extra `&` /
    /// `&mut` layer before passing it to the method.
    #[allow(dead_code)]
    pub(crate) is_ref_impl: bool,
    /// Per-argument `is_mut` flag drained from the resolved method's
    /// parameter signature (`lookup_method_param_is_mut`). Reify zips
    /// this with the reified argument exprs to build [`crate::tir::CallArg`]s
    /// with the same `is_mut` shape annotate produced.
    #[allow(dead_code)]
    pub(crate) param_is_mut: Vec<bool>,
    /// Parameter names in declaration order. Used as substitution keys
    /// when a default references an earlier parameter (`fn f(w, h = w)`).
    #[allow(dead_code)]
    pub(crate) param_names: Vec<String>,
    /// Per-parameter default expression ASTs (`None` for required).
    #[allow(dead_code)]
    pub(crate) param_defaults: Vec<Option<crate::ast::Expr>>,
}

/// Which sub-coercion [`super::super::Elaborator::try_coerce`] applied at
/// a given expression site.
///
/// The variant is what `reify` (Stage 5) needs to pick the same lowering
/// path without re-checking expected-type compatibility; the target type
/// comes alongside on [`CoercionChoice`].
#[derive(Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum CoercionKind {
    /// `42 → i64`, `1.0 → f32`, `0xff_ff_ff_ff → u32`, etc.
    NumericLiteral,
    /// `null → Option<T>` for some unwrapped `T`.
    NullToOption,
    /// A `String` / template literal retagged as a newtype over `String`.
    StringNewtype,
    /// A closure literal retagged as a newtype over its fn-type.
    ClosureToFnNewtype,
    /// A tuple literal lowered through `SequenceLiteralBuilder` (Array
    /// and user-defined sequence types).
    TupleToSequence,
    /// An anonymous struct literal lowered through `KeyValueLiteralBuilder`.
    StructToMap,
}

/// Coercion decision recorded for an expression that
/// [`super::super::Elaborator::try_coerce`] adapted into its expected
/// type. See [`CoercionKind`] for the variants.
#[derive(Clone)]
pub(crate) struct CoercionChoice {
    pub(crate) kind: CoercionKind,
    pub(crate) target_type: TypeId,
}

/// Per-`AstId` type annotations recorded by the body walk.
#[derive(Default, Clone)]
pub(crate) struct TypeAnnotations {
    /// Resolved [`TypeId`] for each local binding, keyed by the binding's
    /// defining [`SymbolKey`]. Populated alongside
    /// [`super::bindings::ModuleBindings::local_symbols`] at every
    /// `record_local_symbol` call. Consumed by LSP inlay hints via
    /// [`crate::semantics::Semantics::local_type_name`] so `let x = 1` can
    /// render the inferred `: i32` annotation without reaching into TIR.
    pub(crate) local_types: IndexMap<SymbolKey, TypeId>,
    /// Resolved [`TypeId`] for every expression visited by
    /// [`super::super::Elaborator::resolve_expr`], keyed by the
    /// expression's [`AstId`]. Populated unconditionally at the end of the
    /// resolver wrapper so every sub-expression — including operands of
    /// binary ops, call arguments, and block trailing values — leaves an
    /// entry. The future `reify` pass (Stage 5) reads this map to set
    /// `TirExpr::type_id` without re-running type inference; LSP hover
    /// may also consult it directly.
    pub(crate) expression_types: IndexMap<AstId, TypeId>,
    /// Method-dispatch decisions recorded for each AST
    /// [`crate::ast::MethodCallExpr`] visited by the body walk, keyed by
    /// the call expression's [`AstId`]. See [`MethodDispatch`] for the
    /// data shape and the recording contract.
    pub(crate) method_dispatch: IndexMap<AstId, MethodDispatch>,
    /// Coercion decisions recorded for each expression that
    /// [`super::super::Elaborator::try_coerce`] adapted into its expected
    /// type, keyed by the source-expression's [`AstId`]. Expressions that
    /// did not need coercion (the resolved type already matched the
    /// expected type, or no `expected_type` was supplied) leave no entry.
    /// See [`CoercionChoice`] / [`CoercionKind`] for the variants.
    pub(crate) coercions: IndexMap<AstId, CoercionChoice>,
    /// Desugar-kind tag for each TIR-direct rewrite site (assert,
    /// matches, comparison chain, for-of, while, compound assignment),
    /// keyed by the enclosing AST node's [`AstId`]. See [`DesugarKind`].
    pub(crate) desugars: IndexMap<AstId, DesugarKind>,
    /// Generic instantiations decided by inference at call / construction
    /// sites (Gap 1 of Stage 5). Keyed by the call expression's, struct
    /// literal's, or variant-ctor's [`AstId`]. See [`GenericInstantiation`].
    ///
    /// `#[allow(dead_code)]` until the call.rs / expr.rs recording call
    /// sites are wired up in a follow-up of Stage 5.
    #[allow(dead_code)]
    pub(crate) generic_instantiations: IndexMap<AstId, GenericInstantiation>,
    /// Capture analysis result for each closure expression (Gap 4 of
    /// Stage 5). Keyed by the [`crate::ast::ClosureExpr`]'s [`AstId`].
    /// See [`ClosureCaptureInfo`].
    pub(crate) closure_captures: IndexMap<AstId, ClosureCaptureInfo>,
    /// Power-assert capture-slot map for each assert statement (Gap 5
    /// of Stage 5). Keyed by the [`crate::ast::AssertStmt`]'s [`AstId`].
    /// See [`AssertCaptureInfo`].
    pub(crate) assert_captures: IndexMap<AstId, AssertCaptureInfo>,
    /// For-of iterator dispatch decisions for the `IntoIterator` path
    /// (Gap 6 of Stage 5). Keyed by the [`crate::ast::ForOfStmt`]'s
    /// [`AstId`]. Tuple and variadic paths are tagged via [`DesugarKind`]
    /// alone and leave no entry here. See [`ForOfIteratorInfo`].
    pub(crate) for_of_iterator: IndexMap<AstId, ForOfIteratorInfo>,
    /// Operator-dispatch decisions for binary / index expressions that
    /// the elaborator lowered to a trait method call (Gap 11 of
    /// Stage 5). Keyed by the [`crate::ast::BinaryExpr`]'s or
    /// [`crate::ast::IndexExpr`]'s [`AstId`]. Absence of an entry
    /// means the elaborator emitted a native
    /// [`crate::tir::TirExprKind::Binary`] /
    /// [`crate::tir::TirExprKind::Index`] for this expression. See
    /// [`OperatorDispatch`].
    pub(crate) operator_dispatch: IndexMap<AstId, OperatorDispatch>,
    /// Handler-binding resolution facts (Gap 13 of Stage 5).
    /// Keyed by the [`crate::ast::EffectHandlerBinding`]'s
    /// [`AstId`]. Carries the list of effects this binding
    /// installs (one per element for explicit form; many for
    /// bundled form where the handler value's type implements
    /// multiple effects), plus the shared `bundle_group` id for
    /// the bundled case. Reify reads this to enumerate the
    /// expanded `TirHandlerBinding`s without re-running
    /// `collect_effect_impls_for_type`.
    ///
    /// `#[allow(dead_code)]` until `reify_with_handler` reads it
    /// and the recording sites in `resolve_with_handler` /
    /// `resolve_handler_binding` are wired through.
    #[allow(dead_code)]
    pub(crate) handler_bindings: IndexMap<AstId, HandlerBindingFacts>,
    /// Impl-block resolution facts (Gap 12 of Stage 5). Keyed by
    /// the [`crate::ast::ImplBlock`]'s [`AstId`]. Carries the
    /// resolved `Self` type, the trait reference's canonical and
    /// mangled forms, the impl's projected
    /// [`crate::tir::TirTypeParam`] list, associated-type bindings
    /// for in-body `Self::X` resolution, the handler-method flag
    /// driving `resume` validation, and the ref-type-impl flag
    /// for `&self` synthesis. See [`ImplFacts`].
    ///
    /// `#[allow(dead_code)]` until the recording sites in the
    /// `Item::Impl` arm of `elaborator.rs` and the reify consumer
    /// in `reify_impl` are wired up; the field is introduced with
    /// the Gap 12 design so the data shape is reviewable
    /// independently.
    #[allow(dead_code)]
    pub(crate) impl_facts: IndexMap<AstId, ImplFacts>,
    /// Resolved `with` clause for each function / method declaration
    /// (Gap of Stage 5). Keyed by the [`crate::ast::Function`]'s or
    /// [`crate::ast::Method`]'s [`AstId`]. The body walk calls
    /// [`super::super::Elaborator::resolve_effects`] while effect
    /// parameters are still in scope and stashes the result here so
    /// reify can drop the `Vec<EffectRef>` straight into
    /// [`crate::tir::TirFunction::effects`] without re-running
    /// effect-param / import / canonicalisation lookups (none of which
    /// reify has the transient `current_effect_param_decls` scope to
    /// reproduce faithfully).
    #[allow(dead_code)]
    pub(crate) function_effects: IndexMap<AstId, Vec<crate::tir::EffectRef>>,
    /// Resolved static-method call dispatch
    /// (`Type::method(args)` / `builtin::fn(args)` shape) recorded by
    /// the body walk. Keyed by the [`crate::ast::CallExpr`]'s [`AstId`].
    /// Carries the resolved [`crate::tir::FunctionRef`] (with mangled
    /// name, `module_source`, `method_info`, and any monomorphisation
    /// info) plus the per-arg `is_mut` flags the elaborator drained
    /// from the looked-up parameter signature. Reify reads this to
    /// reproduce the same TIR `Call` shape without re-running
    /// `locate_static_method_impl` / `MethodName::format_local` and
    /// without consulting the trait-impl index.
    #[allow(dead_code)]
    pub(crate) static_method_dispatch: IndexMap<AstId, StaticMethodDispatch>,
    /// `SequenceLiteralBuilder` coercion data for a tuple literal
    /// coerced into an `Array<T>` / user-defined sequence type. Keyed
    /// by the `Expr::TupleLiteral`'s [`AstId`]. The
    /// `try_coerce_tuple_to_sequence` path produces a desugar block
    /// (`__seq_lit: { let __b = Builder::new_literal(N); __b.push_literal(...); ...; break __seq_lit: __b.build(); }`)
    /// whose shape depends on impl-lookup decisions reify cannot
    /// reproduce from the AST alone. Recording the resolved trait
    /// info here lets reify rebuild the same desugar deterministically
    /// — the `__b` local lands at the same `FunctionContext` index
    /// reify reserves for it (Gap 7 walk-order invariant).
    #[allow(dead_code)]
    pub(crate) sequence_coercions: IndexMap<AstId, SequenceCoercionFacts>,
    /// `KeyValueLiteralBuilder` coercion data for an anonymous
    /// struct literal coerced into a map-style type. Keyed by the
    /// `Expr::StructLiteral`'s [`AstId`]. Counterpart to
    /// [`Self::sequence_coercions`] — the elaborator's
    /// `try_coerce_struct_to_map_inner` records the resolved impl
    /// info so reify rebuilds the `__kv_lit:` desugar block
    /// deterministically.
    #[allow(dead_code)]
    pub(crate) key_value_coercions: IndexMap<AstId, KeyValueCoercionFacts>,
    /// `IndexAssign` trait dispatch decisions for `arr[i] = v` and
    /// `arr[i] OP= v` shapes whose target is an `Expr::Index`. Keyed
    /// by the **inner [`crate::ast::IndexExpr`]'s [`AstId`]** (the
    /// `target` of the surrounding `Expr::Assign` /
    /// `Expr::CompoundAssign`). The recording sites live in
    /// `Elaborator::assign_to_target` so both `Expr::Assign` and the
    /// compound-assign desugar feed the same map. Note that
    /// [`Self::operator_dispatch`] keyed by the same `AstId` carries
    /// the *read-side* `IndexValue` / `Index` dispatch — the two
    /// annotations cohabit because an `Expr::Index` is read in
    /// `let x = arr[i]` and written in `arr[i] = v`, and the
    /// elaborator dispatches each shape to a different trait.
    #[allow(dead_code)]
    pub(crate) index_assign_dispatch: IndexMap<AstId, OperatorDispatch>,
}

/// Resolved `SequenceLiteralBuilder` impl data for a tuple-to-sequence
/// coercion site. See [`TypeAnnotations::sequence_coercions`].
#[derive(Clone)]
#[allow(dead_code)]
pub(crate) struct SequenceCoercionFacts {
    /// The builder struct's [`crate::tir::TypeId`] (e.g.
    /// `Array<i32>`'s `SequenceLiteralBuilder` is `Array<i32>` itself).
    pub(crate) builder_type: crate::tir::TypeId,
    /// The element type each `push_literal` call takes.
    pub(crate) element_type: crate::tir::TypeId,
    /// `self` kind on the resolved `push_literal` method.
    pub(crate) push_self_kind: crate::ast::SelfKind,
    /// Fully resolved trait name (e.g. `"SequenceLiteralBuilder<i32>"`).
    pub(crate) trait_name: String,
    /// The `Builder::build()` call's return type.
    pub(crate) output_type: crate::tir::TypeId,
    /// Module that hosts the impl block.
    pub(crate) impl_module_source: crate::module_source::ModuleSource,
    /// Builder's base struct name (e.g. `"Array"`).
    pub(crate) builder_base_name: String,
    /// Mangled struct name used in `format_local` (e.g. `"Array<i32>"`).
    pub(crate) mangled_builder_name: String,
    /// Type-arg `TypeId`s on the builder (e.g. `[i32]` for `Array<i32>`).
    pub(crate) type_arg_ids: Vec<crate::tir::TypeId>,
    /// Type-arg display names (mangled) parallel to `type_arg_ids`.
    pub(crate) type_arg_names: Vec<String>,
    /// When `Some`, the literal is being coerced into a newtype over
    /// the builder's output — reify wraps the final `__b.build()` call
    /// in a `Cast` to this newtype `TypeId`.
    pub(crate) newtype_cast_to: Option<crate::tir::TypeId>,
}

/// Resolved `KeyValueLiteralBuilder` impl data for an anonymous
/// struct-literal → map coercion site. See
/// [`TypeAnnotations::key_value_coercions`].
#[derive(Clone)]
#[allow(dead_code)]
pub(crate) struct KeyValueCoercionFacts {
    /// The builder struct's [`crate::tir::TypeId`].
    pub(crate) builder_type: crate::tir::TypeId,
    /// The value-side [`crate::tir::TypeId`] each `insert_literal`
    /// call takes.
    pub(crate) value_type: crate::tir::TypeId,
    /// `self` kind on the resolved `insert_literal` method.
    pub(crate) insert_self_kind: crate::ast::SelfKind,
    /// Fully resolved trait name.
    pub(crate) trait_name: String,
    /// The block's resulting type (= `target_type`).
    pub(crate) target_type: crate::tir::TypeId,
    /// Module that hosts the impl block.
    pub(crate) impl_module_source: crate::module_source::ModuleSource,
    /// Builder's base struct name.
    pub(crate) builder_base_name: String,
    /// Mangled struct name used in `format_local`.
    pub(crate) mangled_builder_name: String,
    /// Type-arg `TypeId`s on the builder.
    pub(crate) type_arg_ids: Vec<crate::tir::TypeId>,
    /// Type-arg display names (mangled) parallel to `type_arg_ids`.
    pub(crate) type_arg_names: Vec<String>,
    /// `true` when the trait is `KeyValueLiteralBuilder` (new API
    /// taking a capacity arg + emitting `__b.build()`); `false` for
    /// the legacy `KeyValueLiteral` shape that breaks with `__b`.
    pub(crate) use_new_api: bool,
}

/// Static-method call dispatch decision. See
/// [`TypeAnnotations::static_method_dispatch`].
#[derive(Clone)]
#[allow(dead_code)]
pub(crate) struct StaticMethodDispatch {
    /// The resolved callee — `module_source`, mangled `name`,
    /// `method_info`, `monomorph_info` — as the elaborator constructed
    /// it after impl lookup and mangling.
    pub(crate) function_ref: crate::tir::FunctionRef,
    /// Per-argument `is_mut` flag derived from the resolved parameter
    /// signature (`lookup_static_method_param_is_mut`). Reify zips this
    /// with the reified argument exprs to build [`crate::tir::CallArg`]s
    /// with the same `is_mut` shape annotate produced.
    pub(crate) param_is_mut: Vec<bool>,
}

/// Generic-instantiation decision recorded by the body walk at a call,
/// struct-literal, or variant-construction site. `type_args` is the
/// inferred (or explicitly written) concrete type for each generic
/// parameter in declaration order. `instance_type` is the `TypeId` of
/// the resulting [`crate::tir::ResolvedType::GenericInstance`] (for
/// generic struct/variant types) or the call's monomorphic
/// [`crate::tir::FunctionRef::monomorph_info`]-keyed target — recorded
/// alongside `type_args` so reify can drop straight into the TIR slot
/// without re-running `make_generic_instance` or mangled-name
/// construction.
///
/// `#[allow(dead_code)]` because reify is the consumer (Stage 5).
#[derive(Clone)]
#[allow(dead_code)]
pub(crate) struct GenericInstantiation {
    pub(crate) type_args: Vec<TypeId>,
    pub(crate) instance_type: TypeId,
}

/// A single mutating outer-binding captured by a closure. The closure
/// pre-pass at `closure.rs:140–193` materialises a
/// `let __ref_<var_name> = &mut <var_name>;` in the outer scope before
/// opening the closure body; reify replays the same `add_local` at the
/// same point. The fields below carry every value the replay needs.
#[derive(Clone)]
#[allow(dead_code)]
pub(crate) struct MutCapture {
    /// Original outer-binding name (the source-level identifier).
    pub(crate) var_name: String,
    /// Synthesised reference binding name (`__ref_<var_name>`).
    pub(crate) ref_name: String,
    /// `TypeId` of the inner value (`T`).
    pub(crate) inner_type: TypeId,
    /// `TypeId` of the mut-ref (`&mut T`).
    pub(crate) ref_type: TypeId,
    /// Outer function's local index for the original binding. Reify
    /// recomputes the same index from its own walk (see the
    /// `FunctionContext::locals` walk-order invariant in
    /// `wep-2026-05-26-elaborator-rearchitecture.md` §`Design notes
    /// (Stage 5)`); this field is the cross-check.
    pub(crate) outer_index: u32,
}

/// One entry in the closure's capture list. Mirrors
/// [`crate::tir::TirCapture`] but lives off the TIR so reify produces
/// the same shape from the recorded info.
#[derive(Clone)]
#[allow(dead_code)]
pub(crate) struct CaptureEntry {
    pub(crate) name: String,
    pub(crate) outer_index: u32,
    pub(crate) type_id: TypeId,
    pub(crate) is_mut: bool,
}

/// Closure capture-analysis result recorded by [`super::super::Elaborator::resolve_closure`].
/// Keyed by the closure expression's [`AstId`] in
/// [`TypeAnnotations::closure_captures`].
#[derive(Clone)]
#[allow(dead_code)]
pub(crate) struct ClosureCaptureInfo {
    /// Mut-captures the outer scope must materialise before the closure
    /// body opens, in declaration order. Reify replays each as
    /// `let __ref_<var> = &mut <var>;`.
    pub(crate) mut_captures: Vec<MutCapture>,
    /// Final capture list the closure surfaces to its caller, in the
    /// order `FunctionContext::get_captures` produced.
    pub(crate) captures: Vec<CaptureEntry>,
    /// True when any capture mutates its outer binding. Drives the
    /// `fn mut(...)` vs `fn(...)` choice at the closure type.
    pub(crate) is_mutating: bool,
}

/// One power-assert capture slot — a sub-expression of the assert
/// condition that the [`super::super::assert::CaptureScanner`] flagged
/// for capture as `let __vK = …;` so the panic template can quote its
/// value.
#[derive(Clone)]
#[allow(dead_code)]
pub(crate) struct AssertSlot {
    /// The flagged sub-expression's [`AstId`].
    pub(crate) ast_id: AstId,
    /// The user-facing label the panic template uses for this slot.
    pub(crate) capture_label: String,
}

/// Power-assert capture map recorded by
/// [`super::super::Elaborator::desugar_assert`]. Reify walks the
/// condition AST and consults `slots` to decide which sub-expressions
/// become `let __vK = …;`; only the indices in `emitted_slot_indices`
/// actually produced a binding (slots whose AST evaporated during
/// resolution stay unbound and are skipped by the template).
#[derive(Clone)]
#[allow(dead_code)]
pub(crate) struct AssertCaptureInfo {
    pub(crate) slots: Vec<AssertSlot>,
    pub(crate) emitted_slot_indices: Vec<u32>,
}

/// Handler-binding resolution facts recorded once per
/// [`crate::ast::EffectHandlerBinding`] at annotate time (Gap 13
/// of Stage 5). Reify reads this entry to enumerate the same
/// `TirHandlerBinding` list annotate's combined walk produces,
/// without re-running `collect_effect_impls_for_type` or the
/// explicit-form `trait_env` validation.
#[derive(Clone)]
#[allow(dead_code)]
pub(crate) struct HandlerBindingFacts {
    /// One entry per effect this binding installs. For the
    /// explicit form (`Effect => handler_expr`) this is a
    /// single element; for the bundled form
    /// (`with handler_expr do { ... }`) one element per effect
    /// the handler value's type implements.
    pub(crate) effects: Vec<HandlerEffectEntry>,
    /// Shared `bundle_group` id when this binding came from a
    /// bundled clause. `None` for the explicit form. Reify
    /// writes this onto every emitted `TirHandlerBinding`'s
    /// `bundle_group` field so dispatch synthesis allocates one
    /// shared `__h_<bundle>` local across all the effects this
    /// bundled clause installs.
    pub(crate) bundle_group: Option<u32>,
    /// The handler value's underlying type after reference
    /// peeling. Reify writes this onto every emitted
    /// `TirHandlerBinding`'s `handler_type` field so codegen
    /// routes to the right `impl E for T` methods.
    pub(crate) handler_type: TypeId,
}

/// One effect a handler binding installs. Mirrors the per-effect
/// component the elaborator computes inside
/// `resolve_explicit_handler_binding` /
/// `resolve_bundled_handler_binding` (handlers.rs:108+, 236+).
#[derive(Clone)]
#[allow(dead_code)]
pub(crate) struct HandlerEffectEntry {
    pub(crate) name: String,
    pub(crate) module_source: crate::module_source::ModuleSource,
    pub(crate) trait_type_args: Vec<TypeId>,
}

/// Impl-block resolution facts recorded once per `impl` block at
/// annotate time (Gap 12 of Stage 5). Reify reads this entry
/// keyed by the [`crate::ast::ImplBlock`]'s [`AstId`] and uses
/// every field verbatim — no re-resolution of the impl target,
/// the trait reference, the type params, or the associated
/// types happens inside `reify_impl`.
#[derive(Clone)]
#[allow(dead_code)]
pub(crate) struct ImplFacts {
    /// The impl's `Self` type. For non-generic impls a bare
    /// `Struct { name, module }`; for generic impls a
    /// `GenericInstance` whose `type_args` are the impl's own
    /// `TypeParam` ids in declaration order. Reify uses this to
    /// synthesise `&self` / `&mut self` via `make_ref` /
    /// `make_mut_ref` without re-interning the type-param ids.
    pub(crate) self_type: TypeId,
    /// Full mangled trait name (e.g. `"Stream<u8>"`) — `None` for
    /// inherent impls. Lives on `FunctionRef::method_info`'s
    /// `trait_name` field.
    pub(crate) trait_name_mangled: Option<String>,
    /// Canonical `(declaring_module, base_trait_name)` key —
    /// `None` for inherent impls. Lives on
    /// `LocalMethodName::{base_trait_module, base_trait_name}`
    /// and disambiguates two modules' same-named traits in the
    /// `trait_env` dispatch indices.
    pub(crate) trait_canonical: Option<(crate::module_source::ModuleSource, String)>,
    /// `TirTypeParam` projection of the impl's generic params, in
    /// declaration order, with concrete-typed positions skipped
    /// (e.g. `impl<i32, T>` projects only `T`). Written into
    /// every method's `TirFunction::impl_type_params`.
    pub(crate) impl_type_params: Vec<crate::tir::TirTypeParam>,
    /// Associated-type bindings (`type Output = T;`) resolved
    /// against the impl's type-param scope. Reify writes them
    /// onto each method's `trait_ctx.assoc_type_bindings` so
    /// `Self::X` resolves inside the method body via the shared
    /// type lookup.
    pub(crate) assoc_type_bindings: crate::hashmap::IndexMap<String, TypeId>,
    /// True iff the impl's trait reference names an effect
    /// (`interface`) declaration — i.e. this is an effect handler
    /// impl. Reify writes onto `FunctionContext::in_handler_method`
    /// so `resume` validation matches annotate.
    pub(crate) is_handler_method: bool,
    /// True iff the impl target is `&T` / `&mut T` (ref-type
    /// impl). Method receivers `&self` get an extra `&` layer at
    /// receiver-adjustment time; mirrors Gap 2's per-call
    /// `is_ref_impl` but is decided at impl-block scope.
    pub(crate) is_ref_impl: bool,
}

/// Operator-dispatch decision recorded when the elaborator lowers a
/// binary or index expression to a trait method call (Gap 11 of
/// Stage 5). Reify checks this map before falling back to the native
/// [`crate::tir::TirExprKind::Binary`] / [`crate::tir::TirExprKind::Index`]
/// path.
#[derive(Clone)]
#[allow(dead_code)]
pub(crate) struct OperatorDispatch {
    /// The trait method to dispatch to. Carries the impl block's
    /// module source, mangled name, and `LocalMethodName` metadata
    /// the elaborator already populated.
    pub(crate) function_ref: FunctionRef,
    /// Self-kind of the trait method's receiver. Reify feeds this
    /// (with `is_ref_impl = false` — operator trait methods are
    /// always dispatched on the value type, not on a ref-impl) into
    /// [`super::super::Elaborator::adjust_receiver_for_self_kind_static`].
    pub(crate) self_kind: ast::SelfKind,
    /// Per-argument flag: `true` when the operator's trait parameter
    /// is declared as `&T` / `&mut T` and reify must wrap the
    /// argument in `Unary { Ref }` / `Unary { MutRef }` before
    /// passing it. Indexed in the same order the elaborator's
    /// argument-walk produces (LHS-first for binary; the lone index
    /// for `IndexExpr`).
    pub(crate) arg_ref_wraps: Vec<bool>,
    /// Return type the elaborator resolved for the method call. Pinned
    /// here so reify reads it without re-running impl-table lookups.
    pub(crate) return_type: TypeId,
}

/// `for x of expr` iterator dispatch result (Gap 6 of Stage 5).
/// Recorded only on the iterator path (`DesugarKind::ForOfIterator`);
/// tuple and variadic paths don't dispatch, so they don't record here.
///
/// The two `FunctionRef`s are the same dispatch targets the elaborator
/// resolved through `resolve_method_call_with(method_id: None,
/// call_id: None)`, captured here so reify emits the synthetic calls
/// without re-dispatching.
#[derive(Clone)]
#[allow(dead_code)]
pub(crate) struct ForOfIteratorInfo {
    /// Resolved `IntoIterator::into_iter` dispatch target.
    pub(crate) into_iter: FunctionRef,
    /// Receiver-adjustment kind for the `.into_iter()` call.
    pub(crate) into_iter_self_kind: ast::SelfKind,
    /// True when the `into_iter` impl was found on a reference-type
    /// impl (cf. [`MethodDispatch::is_ref_impl`]).
    pub(crate) into_iter_is_ref_impl: bool,
    /// Resolved `Iterator::next` dispatch target.
    pub(crate) next: FunctionRef,
    /// Receiver-adjustment kind for the `.next()` call.
    pub(crate) next_self_kind: ast::SelfKind,
    /// True when the `next` impl was found on a reference-type impl.
    pub(crate) next_is_ref_impl: bool,
    /// Item type — what the loop variable is bound to. Reify uses this
    /// to type-annotate the synthesised `let <var> = …;`.
    pub(crate) item_type: TypeId,
    /// Iterator type — the resolved return type of
    /// `iterable.into_iter()` (`info.into_iter` returns this).
    /// Reify uses this to type the synthesised iterator local
    /// `let mut __iter_N: <iter_type> = …;` without re-running
    /// method dispatch.
    pub(crate) iter_type: TypeId,
}

/// Which TIR-direct desugar path the body walk took at a source-level
/// rewrite site. The variants enumerate every surface form whose
/// lowering bypasses synthetic AST construction (see the LSP-friendly
/// compiler architecture note in
/// `wado-compiler/CLAUDE.md`); the future `reify` pass (Stage 5) reads
/// this tag to pick the same expansion without re-deciding the shape.
#[derive(Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum DesugarKind {
    /// `assert cond[, msg];` → power-assert capture + guard expansion.
    Assert,
    /// `expr matches { PATTERN }` → two-arm `match` expression.
    Matches,
    /// `a < b < c` → `(a < b) && (b < c)` with middle-term let bindings.
    /// Only recorded when the chain has 2+ comparisons (single
    /// comparisons collapse to a plain `Binary` and are not tagged).
    ComparisonChain,
    /// `for let v of tuple { body }` → unrolled body per element.
    ForOfTuple,
    /// `for let v of variadic_tuple { body }` → deferred `VariadicForOf`
    /// TIR node consumed by monomorphization.
    ForOfVariadic,
    /// `for let v of expr { body }` → `IntoIterator` / `next()` loop.
    /// Not recorded when the iterable fails the `IntoIterator` check.
    ForOfIterator,
    /// `for (init; cond; update) { body }` → labeled-block + loop with
    /// init, conditional break, body, update (the C-style for desugar).
    CStyleFor,
    /// `while cond { body }` → `loop { if !cond { break } body }`.
    While,
    /// `while let chain { body }` → let-chain `match` with break arm.
    WhileLetChain,
    /// `if let chain { … } else { … }` → let-chain `match` with the
    /// else block as the wildcard arm.
    IfLetChain,
    /// `x += y` (and other compound ops) → `x = x + y` style rewrite.
    CompoundAssign,
    /// `container[i].method()` → materialise `let __index_mut_val =
    /// &mut container[i];` + dispatch the method through it
    /// (`method_lookup.rs::try_resolve_index_mut_method_call`). Tagged
    /// on the [`crate::ast::MethodCallExpr`]'s [`AstId`]; the receiver
    /// `IndexExpr` keeps its own `expression_types` entry so reify
    /// types the synthesised initialiser correctly.
    IndexMutMethodCall,
    /// `Newtype::from(x)` where `x` is already of the newtype's base
    /// type — the elaborator collapses the call to `x` itself. Reify
    /// reads this tag on the outer `Call`'s [`AstId`] and emits the
    /// inner argument's TIR directly, skipping the call construction.
    NewtypeFromCollapse,
}
