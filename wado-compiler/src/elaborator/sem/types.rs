//! [`TypeAnnotations`] — the per-[`crate::ast::AstId`] *decisions* the body walk
//! made and [`super::super::reify`] reads in lieu of re-running inference:
//! resolved types, dispatch targets, coercions, desugar tags, generic
//! instantiations, capture tables. A bare `AstId` keys them because ids are
//! globally unique, so cross-module inlined AST cannot collide with its
//! consumer's. Facts derivable from the AST alone belong on
//! [`crate::ast_index::AstIndex`], decl-level ones on [`super::decls::ModuleDecls`].

use crate::ast::{self, AstId};
use crate::hashmap::IndexMap;
use crate::name::Receiver;
use crate::tir::{FunctionRef, TypeId};

/// Method-dispatch decision for a [`crate::ast::MethodCallExpr`]:
/// `function_ref` is the resolved target, so reify emits the call without
/// re-running trait lookup or mangling, and `self_kind` plus `is_ref_impl` are
/// what `adjust_receiver_for_self_kind` needs — an `impl Trait for &T` takes an
/// extra `&`. Short-circuiting paths leave no entry: they rewrite the call into
/// a shape reify recognises from the receiver type alone.
#[derive(Clone)]
pub(crate) struct MethodDispatch {
    pub(crate) function_ref: FunctionRef,
    pub(crate) self_kind: ast::SelfKind,
    /// True when the resolved method's impl was found on a reference type
    /// (`impl Trait for &T`). Reify wraps the receiver in an extra `&` /
    /// `&mut` layer before passing it to the method.
    pub(crate) is_ref_impl: bool,
    /// Per-argument `is_mut` flag drained from the resolved method's
    /// parameter signature (`lookup_method_param_is_mut`). Reify zips
    /// this with the reified argument exprs to build [`crate::tir::CallArg`]s
    /// with the same `is_mut` shape annotate produced.
    pub(crate) param_is_mut: Vec<bool>,
    /// Parameter names in declaration order. Used as substitution keys
    /// when a default references an earlier parameter (`fn f(w, h = w)`).
    pub(crate) param_names: Vec<String>,
    /// Per-parameter default expression ASTs (`None` for required).
    pub(crate) param_defaults: Vec<Option<crate::ast::Expr>>,
    /// The resolved method's return [`TypeId`] — the authoritative result
    /// type of the call. Reify uses this for the call's
    /// `type_id` rather than the per-`AstId` `expression_types` entry,
    /// which can carry a stale/wrong type for the call site (a unit
    /// method whose `expression_types` slot was recorded as another
    /// type makes reify emit a spurious `drop` of a value-less call →
    /// Wasm stack underflow).
    pub(crate) return_type: TypeId,
    /// Method-level type args for the call node, explicit turbofish and
    /// inference-recovered alike; the monomorphizer keys off this to queue
    /// `Struct^Trait::method<Args>` instances. Kept separate from
    /// `function_ref.monomorph_info.method_type_args`, which the blanket-impl
    /// branch leaves empty even for a turbofish call — reify needs an un-zeroed
    /// copy rather than re-resolving the AST list against the current scope.
    pub(crate) method_type_args: Vec<TypeId>,
    /// True when the method takes its receiver `self` by value, so the call
    /// transfers ownership of the receiver. The resource move check reads this
    /// to flag a use of the receiver binding after a consuming call.
    pub(crate) consumes_self: bool,
}

/// Which sub-coercion [`super::super::Elaborator::try_coerce`] applied at
/// a given expression site.
///
/// The variant is what `reify` needs to pick the same lowering
/// path without re-checking expected-type compatibility; the target type
/// comes alongside on [`CoercionChoice`].
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CoercionKind {
    /// `42 → i64`, `1.0 → f32`, `0xff_ff_ff_ff → u32`, etc.
    NumericLiteral,
    /// `null → Option<T>` for some unwrapped `T`.
    NullToOption,
    /// A `String` / template literal retagged as a newtype over `String`.
    StringNewtype,
    /// A `b"..."` / `#include_bytes` byte literal (default type `ByteList`)
    /// retagged as another type whose ultimate base is `List<u8>` — e.g.
    /// `let x: List<u8> = b"..."`. Free: the repr is identical either way.
    BytesNewtype,
    /// A closure literal retagged as a newtype over its fn-type.
    ClosureToFnNewtype,
    /// A tuple literal lowered through `SequenceLiteralBuilder` (List
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
    /// Resolved [`TypeId`] per local binding, keyed by its defining [`AstId`]
    /// and recorded alongside `local_symbols`. LSP inlay hints read it so
    /// `let x = 1` renders `: i32` without reaching into TIR. Part of
    /// [`ElementOverlay`]: a tuple-`for-of` body rebinds one pattern `AstId` to
    /// a different type per iteration, so each iteration's value stays pinned to
    /// its own overlay instead of overwriting the base map.
    pub(crate) local_types: IndexMap<AstId, TypeId>,
    /// Resolved [`TypeId`] for every expression visited by
    /// [`super::super::Elaborator::resolve_expr`], keyed by the
    /// expression's [`AstId`]. Populated unconditionally at the end of the
    /// resolver wrapper so every sub-expression — including operands of
    /// binary ops, call arguments, and block trailing values — leaves an
    /// entry. Reify reads this map to set
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
    /// sites. Keyed by the call expression's, struct
    /// literal's, or variant-ctor's [`AstId`]. Recorded by
    /// `record_generic_instantiation*` (call.rs / expr.rs / stmt.rs); read
    /// by reify via `ann_generic_instantiations`. See
    /// [`GenericInstantiation`].
    pub(crate) generic_instantiations: IndexMap<AstId, GenericInstantiation>,
    /// Capture analysis result for each closure expression. Keyed by the
    /// [`crate::ast::ClosureExpr`]'s [`AstId`].
    /// See [`ClosureCaptureInfo`].
    pub(crate) closure_captures: IndexMap<AstId, ClosureCaptureInfo>,
    /// Resolved (type-arg-substituted) parameter types for a free /
    /// imported function call, keyed by the call expression's [`AstId`].
    /// Reify uses these to drive per-argument expected types — chiefly so
    /// a closure-literal argument coerced to a `fn`-typed (or `fn`-newtype)
    /// parameter sees the function signature, inferring unannotated closure
    /// params and producing the functor specialization the call site needs.
    pub(crate) call_param_types: IndexMap<AstId, Vec<TypeId>>,
    /// Power-assert capture-slot map for each assert statement. Keyed by
    /// the [`crate::ast::AssertStmt`]'s [`AstId`].
    /// See [`AssertCaptureInfo`].
    pub(crate) assert_captures: IndexMap<AstId, AssertCaptureInfo>,
    /// For-of iterator dispatch decisions for the `IntoIterator` path
    /// Keyed by the [`crate::ast::ForOfStmt`]'s
    /// [`AstId`]. Tuple and variadic paths are tagged via [`DesugarKind`]
    /// alone and leave no entry here. See [`ForOfIteratorInfo`].
    pub(crate) for_of_iterator: IndexMap<AstId, ForOfIteratorInfo>,
    /// Operator-dispatch decisions for binary / index expressions that
    /// the elaborator lowered to a trait method call. Keyed by the
    /// [`crate::ast::BinaryExpr`]'s or
    /// [`crate::ast::IndexExpr`]'s [`AstId`]. Absence of an entry
    /// means the elaborator emitted a native
    /// [`crate::tir::TirExprKind::Binary`] /
    /// [`crate::tir::TirExprKind::Index`] for this expression. See
    /// [`OperatorDispatch`].
    pub(crate) operator_dispatch: IndexMap<AstId, OperatorDispatch>,
    /// Handler-binding resolution facts.
    /// Keyed by the [`crate::ast::EffectHandlerBinding`]'s
    /// [`AstId`]. Carries the list of effects this binding
    /// installs (one per element for explicit form; many for
    /// bundled form where the handler value's type implements
    /// multiple effects), plus the shared `bundle_group` id for
    /// the bundled case. Reify reads this to enumerate the
    /// expanded `TirHandlerBinding`s without re-running
    /// `collect_effect_impls_for_type`.
    pub(crate) handler_bindings: IndexMap<AstId, HandlerBindingFacts>,
    /// Impl-block resolution facts keyed by the [`crate::ast::ImplBlock`]'s
    /// [`AstId`] — resolved `Self` type, canonical and mangled trait reference,
    /// projected type-param list, associated-type bindings, and the handler /
    /// ref-impl flags. See [`ImplFacts`]. Recorded in the `Item::Impl` arm,
    /// read by `reify_impl`.
    pub(crate) impl_facts: IndexMap<AstId, ImplFacts>,
    /// Resolved `with` clause per function / method declaration. The body walk
    /// resolves it while the effect parameters are still in scope and stashes
    /// the result here, so reify drops the `Vec<EffectRef>` straight into
    /// [`crate::tir::TirFunction::effects`] — it has no
    /// `current_effect_param_decls` scope to redo the lookup faithfully.
    pub(crate) function_effects: IndexMap<AstId, Vec<crate::tir::EffectRef>>,
    /// Declared (pre-erasure) return [`TypeId`] for every `async`
    /// function / method, keyed by the function's [`AstId`]. An async
    /// function's wasm-level `return_type` is erased to `()` (the value
    /// travels via `task return`), so reify cannot recover the real type
    /// from `function_return_types` (which records the erased unit).
    /// reify reads this to set `TirFunction::task_return_type` — needed
    /// for resource-store inference over the return type (e.g. an async
    /// `handle` returning `Result<Response, _>` must surface `Response`).
    pub(crate) function_task_returns: IndexMap<AstId, TypeId>,
    /// Resolved static-method call dispatch (`Type::method(args)` /
    /// `builtin::fn(args)`), keyed by the [`crate::ast::CallExpr`]'s [`AstId`]:
    /// the resolved [`crate::tir::FunctionRef`] plus the per-arg `is_mut` flags
    /// drained from the looked-up signature, so reify rebuilds the `Call` without
    /// `locate_static_method_impl` or the trait-impl index.
    pub(crate) static_method_dispatch: IndexMap<AstId, StaticMethodDispatch>,
    /// `SequenceLiteralBuilder` coercion data for a tuple literal coerced into a
    /// `List<T>` or user-defined sequence, keyed by the `Expr::TupleLiteral`'s
    /// [`AstId`]. The desugar block `try_coerce_tuple_to_sequence` produces
    /// depends on impl-lookup decisions reify cannot redo from the AST; the
    /// resolved trait info here is what makes its `__b` local land at the same
    /// `FunctionContext` index.
    pub(crate) sequence_coercions: IndexMap<AstId, SequenceCoercionFacts>,
    /// `KeyValueLiteralBuilder` coercion data for an anonymous
    /// struct literal coerced into a map-style type. Keyed by the
    /// `Expr::StructLiteral`'s [`AstId`]. Counterpart to
    /// [`Self::sequence_coercions`] — the elaborator's
    /// `try_coerce_struct_to_map_inner` records the resolved impl
    /// info so reify rebuilds the `__kv_lit:` desugar block
    /// deterministically.
    pub(crate) key_value_coercions: IndexMap<AstId, KeyValueCoercionFacts>,
    /// `From<T>::from` call facts recorded at every site that synthesises
    /// a conversion call: the `?` operator's err-arm conversion, and the
    /// bodyless-impl static-call inline path. Keyed by the caller's
    /// [`crate::ast::AstId`] (the `?` expr / static-call expr). See
    /// [`FromCallFacts`].
    pub(crate) from_call_facts: IndexMap<AstId, FromCallFacts>,
    /// `IndexAssign` dispatch for `arr[i] = v` and `arr[i] OP= v`, keyed by the
    /// *inner* [`crate::ast::IndexExpr`]'s [`AstId`] and recorded in
    /// `assign_to_target` so both shapes feed one map.
    /// [`Self::operator_dispatch`] cohabits under the same key with the
    /// read-side `IndexValue` / `Index` dispatch: an `Expr::Index` is read in
    /// `let x = arr[i]` and written in `arr[i] = v`, via different traits.
    pub(crate) index_assign_dispatch: IndexMap<AstId, OperatorDispatch>,
    /// Per-element annotation overlays for compile-time-unrolled tuple `for-of`
    /// loops: one outer entry per *instantiation* in walk order (a nested for-of
    /// is instantiated once per outer element), one inner [`ElementOverlay`] per
    /// tuple element. The body is a single sub-tree with fixed `AstId`s resolved
    /// once per element in a different type context, so without the overlay only
    /// the last element's facts would survive.
    pub(crate) tuple_overlays: IndexMap<AstId, Vec<Vec<ElementOverlay>>>,
    /// Impl-level type parameters as `Elaborator::resolve_method` (the
    /// battle-tested original path) computed them, keyed per impl-method
    /// `AstId`. Reify reads this instead of recomputing the
    /// impl-type-param scheme with its own logic — the single source of truth
    /// for the scheme is the elaborator. Reify reads it only for
    /// explicitly-written methods (unique key); default-method bodies land
    /// under their owning module.
    pub(crate) method_impl_type_params: IndexMap<AstId, Vec<crate::tir::TirTypeParam>>,
    /// Resolved parameter types per function/method `AstId`, in declaration
    /// order (for impl methods, including the receiver `&self` → `&Self`), as
    /// `resolve_function` / `resolve_method` resolved them. Reify reads these
    /// instead of re-resolving each param.
    pub(crate) fn_param_types: IndexMap<AstId, Vec<crate::tir::TypeId>>,
    /// Resolved (post-async-erasure) return type per function/method `AstId`.
    /// Reify reads it instead of re-resolving the return annotation.
    pub(crate) fn_return_types: IndexMap<AstId, crate::tir::TypeId>,
    /// Resolved operation signatures per effect / resource decl `AstId`
    /// (params, return type, `cm` name), as the combined walk resolved them
    /// with the decl's type-param / `Self` scope in place. Reify reads these
    /// instead of re-resolving the op signatures itself.
    pub(crate) effect_ops: IndexMap<AstId, Vec<crate::tir::TirEffectOp>>,
    /// TIR type params per declaration `AstId` (function, method, struct,
    /// variant), with each `default` resolved while the decl's type-param scope
    /// was alive (so a default referencing an earlier param resolves
    /// correctly). Effect / `fn`-bound params are filtered out and indices are
    /// dense. Reify reads these instead of re-projecting / re-resolving them
    /// after its own scope is torn down. `AstId` is dense per module across all
    /// item kinds, so function and decl entries never collide.
    pub(crate) decl_type_params: IndexMap<AstId, Vec<crate::tir::TirTypeParam>>,
    /// Per-impl-method mangled / display names as `resolve_method` computed
    /// them (`MethodName::format_local(struct_name, trait_name, method_name)`
    /// and the trait-omitted display form). Reify reads these instead of
    /// re-running `format_local` against the impl facts' `struct_name`.
    pub(crate) method_names: IndexMap<AstId, MethodNames>,
    /// Resolved `let x: T = …` whole-pattern annotation, keyed by the `LetStmt`'s
    /// `AstId`. A simple binding also has it in `local_types`, but a
    /// destructuring pattern's binding ids carry per-element types, leaving the
    /// whole-pattern annotation nowhere else to land. Part of [`ElementOverlay`]
    /// for the same reason as `local_types`: one `LetStmt` id is shared across
    /// every iteration of a tuple-`for-of` unrolling.
    pub(crate) let_annotated_types: IndexMap<AstId, TypeId>,
    /// Resolved field types per struct decl `AstId`, in declaration order, as
    /// `resolve_struct` produced them with the type-param scope in place. Reify
    /// reads these rather than `tysys.all_struct_fields`, which is seeded by the
    /// static decl-field pass — that runs before import scopes exist and cannot
    /// follow `pub use` chains, so a field typed by a re-exported decl lands
    /// there as UNKNOWN.
    pub(crate) struct_field_types: IndexMap<AstId, Vec<crate::tir::TypeId>>,
    /// Place classification for each identifier that resolves to one — local,
    /// `&mut`-deref-capture, or global — so `assign_to_target` can validate
    /// l-values and global mutability without reading the now-placeholder
    /// resolved `target.kind`. An ident resolving to a function, variant, enum,
    /// flags or constant leaves no entry and is not an l-value. No overlay
    /// needed: a place kind does not depend on a for-of element type.
    pub(crate) assign_places: IndexMap<AstId, AssignPlace>,
}

/// Assignment-target place classification recorded for an identifier by
/// [`super::super::Elaborator::resolve_ident`]. See
/// [`TypeAnnotations::assign_places`].
#[derive(Clone)]
pub(crate) enum AssignPlace {
    /// A function-frame local — always a valid l-value.
    Local,
    /// A `&mut`-captured outer binding accessed through `*__ref` inside a
    /// closure body. Assignable iff the captured reference is `&mut T`
    /// (`through_mut_ref`); a shared-`&` capture is not assignable.
    DerefCapture { through_mut_ref: bool },
    /// A module global. Carries the resolved name (for the immutable-global
    /// diagnostic) and mutability so the assign path validates the write and
    /// projects the `GlobalVarSet`. The module source / original name needed
    /// to rebuild the `GlobalVarSet` are re-derived by reify from the AST, so
    /// they are not stored here.
    Global { name: String, mutable: bool },
}

/// One tuple-`for-of` element's slice of the body-level annotation maps.
///
/// Holds exactly the `AstId`-keyed maps that can appear inside a for-of
/// body and vary per element. Decl-level maps (`impl_facts`,
/// `function_effects`, `function_task_returns`, `handler_bindings`)
/// cannot occur inside an expression body and are excluded. Captured by
/// [`super::super::Elaborator::resolve_tuple_for_of`]; consumed by reify
/// via its overlay-stack accessors.
#[derive(Default, Clone)]
pub(crate) struct ElementOverlay {
    pub(crate) expression_types: IndexMap<AstId, TypeId>,
    pub(crate) local_types: IndexMap<AstId, TypeId>,
    pub(crate) let_annotated_types: IndexMap<AstId, TypeId>,
    pub(crate) method_dispatch: IndexMap<AstId, MethodDispatch>,
    pub(crate) coercions: IndexMap<AstId, CoercionChoice>,
    pub(crate) desugars: IndexMap<AstId, DesugarKind>,
    pub(crate) generic_instantiations: IndexMap<AstId, GenericInstantiation>,
    pub(crate) closure_captures: IndexMap<AstId, ClosureCaptureInfo>,
    pub(crate) call_param_types: IndexMap<AstId, Vec<TypeId>>,
    pub(crate) assert_captures: IndexMap<AstId, AssertCaptureInfo>,
    pub(crate) for_of_iterator: IndexMap<AstId, ForOfIteratorInfo>,
    pub(crate) operator_dispatch: IndexMap<AstId, OperatorDispatch>,
    pub(crate) static_method_dispatch: IndexMap<AstId, StaticMethodDispatch>,
    pub(crate) sequence_coercions: IndexMap<AstId, SequenceCoercionFacts>,
    pub(crate) key_value_coercions: IndexMap<AstId, KeyValueCoercionFacts>,
    pub(crate) index_assign_dispatch: IndexMap<AstId, OperatorDispatch>,
}

/// Pre-loop lengths of the per-element overlay maps, snapshotted before a
/// tuple `for-of` unrolls its body. Used by
/// [`TypeAnnotations::split_off_overlay`] to peel each element's freshly
/// recorded entries off the tail.
#[derive(Clone, Copy)]
pub(crate) struct ElementOverlayLens {
    expression_types: usize,
    local_types: usize,
    let_annotated_types: usize,
    method_dispatch: usize,
    coercions: usize,
    desugars: usize,
    generic_instantiations: usize,
    closure_captures: usize,
    call_param_types: usize,
    assert_captures: usize,
    for_of_iterator: usize,
    operator_dispatch: usize,
    static_method_dispatch: usize,
    sequence_coercions: usize,
    key_value_coercions: usize,
    index_assign_dispatch: usize,
}

impl TypeAnnotations {
    /// Snapshot the current lengths of the per-element overlay maps, taken
    /// right before a tuple `for-of` resolves its first element's body.
    pub(crate) fn overlay_base_lens(&self) -> ElementOverlayLens {
        ElementOverlayLens {
            expression_types: self.expression_types.len(),
            local_types: self.local_types.len(),
            let_annotated_types: self.let_annotated_types.len(),
            method_dispatch: self.method_dispatch.len(),
            coercions: self.coercions.len(),
            desugars: self.desugars.len(),
            generic_instantiations: self.generic_instantiations.len(),
            closure_captures: self.closure_captures.len(),
            call_param_types: self.call_param_types.len(),
            assert_captures: self.assert_captures.len(),
            for_of_iterator: self.for_of_iterator.len(),
            operator_dispatch: self.operator_dispatch.len(),
            static_method_dispatch: self.static_method_dispatch.len(),
            sequence_coercions: self.sequence_coercions.len(),
            key_value_coercions: self.key_value_coercions.len(),
            index_assign_dispatch: self.index_assign_dispatch.len(),
        }
    }

    /// Peel the entries recorded since `base` off the tail of each
    /// per-element overlay map into an [`ElementOverlay`], truncating each
    /// map back to `base` so the next unrolled element records from a
    /// clean slate. `IndexMap::split_off` does both in one step: it
    /// returns the `[base..]` tail and leaves `self` holding `[0..base)`.
    ///
    /// Truncation is what makes per-element capture correct for
    /// conditionally-recorded maps: without it, an entry recorded for one
    /// element but not the next would linger with a stale value.
    pub(crate) fn split_off_overlay(&mut self, base: ElementOverlayLens) -> ElementOverlay {
        ElementOverlay {
            expression_types: self.expression_types.split_off(base.expression_types),
            local_types: self.local_types.split_off(base.local_types),
            let_annotated_types: self.let_annotated_types.split_off(base.let_annotated_types),
            method_dispatch: self.method_dispatch.split_off(base.method_dispatch),
            coercions: self.coercions.split_off(base.coercions),
            desugars: self.desugars.split_off(base.desugars),
            generic_instantiations: self
                .generic_instantiations
                .split_off(base.generic_instantiations),
            closure_captures: self.closure_captures.split_off(base.closure_captures),
            call_param_types: self.call_param_types.split_off(base.call_param_types),
            assert_captures: self.assert_captures.split_off(base.assert_captures),
            for_of_iterator: self.for_of_iterator.split_off(base.for_of_iterator),
            operator_dispatch: self.operator_dispatch.split_off(base.operator_dispatch),
            static_method_dispatch: self
                .static_method_dispatch
                .split_off(base.static_method_dispatch),
            sequence_coercions: self.sequence_coercions.split_off(base.sequence_coercions),
            key_value_coercions: self.key_value_coercions.split_off(base.key_value_coercions),
            index_assign_dispatch: self
                .index_assign_dispatch
                .split_off(base.index_assign_dispatch),
        }
    }
}

/// Mangled and display names for one impl method, as
/// `Elaborator::resolve_method` computes them. Display omits the trait
/// (`Struct::method`); mangled includes it
/// (`Struct^Trait::method`). See [`TypeAnnotations::method_names`].
#[derive(Clone)]
pub(crate) struct MethodNames {
    pub(crate) display: String,
    pub(crate) mangled: String,
}

/// Resolved `From<T>::from` call facts recorded at every site that
/// invokes the conversion: the `?` operator's error-arm conversion
/// (`expr.rs:resolve_question_mark_result`), the bodyless
/// `impl From<X> for T;` static-call inline (`call.rs` /
/// `method_call.rs`). Reify reads these to rebuild the same
/// `TirExprKind::Call` without re-walking loaded modules to find the
/// impl's home or re-mangling the method name.
#[derive(Clone)]
pub(crate) struct FromCallFacts {
    /// Module that hosts the `impl From<From> for Target` block (or
    /// the auto-derived synthesis site).
    pub(crate) module_source: crate::module_source::ModuleSource,
    /// `MethodName::format_local(target_name, Some(from_trait), "from")` —
    /// the mangled `Target^From<From>::from` name the monomorphizer
    /// keys on.
    pub(crate) mangled_name: String,
    /// `type_name(target_type)` — the call's struct prefix
    /// (`LocalMethodName::struct_name` / `base_struct_name`).
    pub(crate) target_name: crate::name::FqTypeName,
    /// `type_name(from_type)` — the conversion source type's name,
    /// used to build `LocalMethodName::trait_name`'s `From<…>` form.
    pub(crate) from_name: String,
    /// The `From` trait, named by the module that declares it.
    pub(crate) from_trait_name: crate::name::FqTraitName,
}

/// Resolved `SequenceLiteralBuilder` impl data for a tuple-to-sequence
/// coercion site. See [`TypeAnnotations::sequence_coercions`].
#[derive(Clone)]
pub(crate) struct SequenceCoercionFacts {
    /// The builder struct's [`crate::tir::TypeId`] (e.g.
    /// `List<i32>`'s `SequenceLiteralBuilder` is `List<i32>` itself).
    pub(crate) builder_type: crate::tir::TypeId,
    /// The element type each `push_literal` call takes.
    pub(crate) element_type: crate::tir::TypeId,
    /// `self` kind on the resolved `push_literal` method.
    pub(crate) push_self_kind: crate::ast::SelfKind,
    /// The builder trait, named by the module that declares it (e.g.
    /// `core:prelude/list.wado/SequenceLiteralBuilder<i32>`).
    pub(crate) trait_name: crate::name::FqTraitName,
    /// The `Builder::build()` call's return type.
    pub(crate) output_type: crate::tir::TypeId,
    /// Module that hosts the impl block.
    pub(crate) impl_module_source: crate::module_source::ModuleSource,
    /// Builder's base struct name, fq (e.g. `core:prelude/list.wado/List`).
    pub(crate) builder_base_name: crate::name::FqTypeName,
    /// Type-arg `TypeId`s on the builder (e.g. `[i32]` for `List<i32>`).
    pub(crate) type_arg_ids: Vec<crate::tir::TypeId>,
    /// Type-arg names parallel to `type_arg_ids`, kept structured.
    pub(crate) type_arg_names: Vec<crate::name::FqTypeName>,
    /// When `Some`, the literal is being coerced into a newtype over
    /// the builder's output — reify wraps the final `__b.build()` call
    /// in a `Cast` to this newtype `TypeId`.
    pub(crate) newtype_cast_to: Option<crate::tir::TypeId>,
    /// `Builder::new_literal` call's mangled name (the one
    /// `MethodName::format_local(mangled_builder_name, trait_name,
    /// "new_literal")` produces). Recorded so reify reads it instead
    /// of running its own `format_local`.
    pub(crate) new_mangled_name: String,
    /// `Builder::push_literal` call's mangled name.
    pub(crate) push_mangled_name: String,
    /// `Builder::build` call's mangled name.
    pub(crate) build_mangled_name: String,
}

impl SequenceCoercionFacts {
    /// Recompute the type-argument names and the builder's per-method mangled
    /// names from `type_arg_ids`. The names are a function of the arguments,
    /// so anything that changes an argument — the module-end sweep, once an
    /// inference variable is solved — calls this instead of rebuilding them
    /// itself and risking one it forgot.
    pub(crate) fn remangle(&mut self, tt: &crate::tir::TypeTable) {
        self.type_arg_names = self
            .type_arg_ids
            .iter()
            .map(|&t| tt.fq_type_name(t))
            .collect();
        let builder = self
            .builder_base_name
            .clone()
            .with_args(self.type_arg_names.clone());
        let name = |method: &str| {
            crate::name::MethodName::format_local(&builder, Some(&self.trait_name), method)
        };
        self.new_mangled_name = name("new_literal");
        self.push_mangled_name = name("push_literal");
        self.build_mangled_name = name("build");
    }
}

/// Resolved `KeyValueLiteralBuilder` impl data for an anonymous
/// struct-literal → map coercion site. See
/// [`TypeAnnotations::key_value_coercions`].
#[derive(Clone)]
pub(crate) struct KeyValueCoercionFacts {
    /// The builder struct's [`crate::tir::TypeId`].
    pub(crate) builder_type: crate::tir::TypeId,
    /// The value-side [`crate::tir::TypeId`] each `insert_literal`
    /// call takes.
    pub(crate) value_type: crate::tir::TypeId,
    /// `self` kind on the resolved `insert_literal` method.
    pub(crate) insert_self_kind: crate::ast::SelfKind,
    /// The builder trait, named by the module that declares it.
    pub(crate) trait_name: crate::name::FqTraitName,
    /// The block's resulting type (= `target_type`).
    pub(crate) target_type: crate::tir::TypeId,
    /// Module that hosts the impl block.
    pub(crate) impl_module_source: crate::module_source::ModuleSource,
    /// Builder's base struct name, fq.
    pub(crate) builder_base_name: crate::name::FqTypeName,
    /// Type-arg `TypeId`s on the builder.
    pub(crate) type_arg_ids: Vec<crate::tir::TypeId>,
    /// Type-arg names parallel to `type_arg_ids`, kept structured.
    pub(crate) type_arg_names: Vec<crate::name::FqTypeName>,
    /// `true` when the trait is `KeyValueLiteralBuilder` (new API
    /// taking a capacity arg + emitting `__b.build()`); `false` for
    /// the legacy `KeyValueLiteral` shape that breaks with `__b`.
    pub(crate) use_new_api: bool,
    /// `Builder::new_literal` call's mangled name. Recorded so reify reads it
    /// instead of running its own `MethodName::format_local`.
    pub(crate) new_mangled_name: String,
    /// `Builder::insert_literal` call's mangled name.
    pub(crate) insert_mangled_name: String,
    /// `Builder::insert_all` call's mangled name, for a `{ ..base, … }` spread.
    pub(crate) insert_all_mangled_name: String,
    /// `Builder::build` call's mangled name. `None` under the legacy API
    /// where the block breaks with `__b` directly.
    pub(crate) build_mangled_name: Option<String>,
}

impl KeyValueCoercionFacts {
    /// [`SequenceCoercionFacts::remangle`] for the key-value builder.
    pub(crate) fn remangle(&mut self, tt: &crate::tir::TypeTable) {
        self.type_arg_names = self
            .type_arg_ids
            .iter()
            .map(|&t| tt.fq_type_name(t))
            .collect();
        let builder = self
            .builder_base_name
            .clone()
            .with_args(self.type_arg_names.clone());
        let name = |method: &str| {
            crate::name::MethodName::format_local(&builder, Some(&self.trait_name), method)
        };
        self.new_mangled_name = name("new_literal");
        self.insert_mangled_name = name("insert_literal");
        self.insert_all_mangled_name = name("insert_all");
        self.build_mangled_name = self.build_mangled_name.as_ref().map(|_| name("build"));
    }
}

/// Static-method call dispatch decision. See
/// [`TypeAnnotations::static_method_dispatch`].
#[derive(Clone)]
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
    /// The exact `type_args` the production builder put on the resulting
    /// `TirExprKind::Call`. For a static method on a generic struct the
    /// impl (struct) type args live in `function_ref.monomorph_info`, so
    /// this list carries only the method-level type args (often empty);
    /// for a free generic function it carries the function's type args.
    /// Reify replays this verbatim instead of re-deriving from
    /// `generic_instantiations`, which would (wrongly) feed the impl args
    /// in as method-level type args and mangle `Container<i32>::make` as
    /// `Container::make<i32>`.
    pub(crate) type_args: Vec<crate::tir::TypeId>,
    /// The callee's `(param_name, default_expr)` list in declaration order,
    /// used by reify to pad trailing arguments the call omitted. Empty when
    /// the method declares no defaults (or for variant / builtin dispatches).
    pub(crate) param_defaults: Vec<(String, Option<crate::ast::Expr>)>,
    /// The callee's resolved parameter types in declaration order, which reify
    /// needs to type a default it materializes — a default on a trait method
    /// has no body for annotate to walk, so nothing recorded its type.
    pub(crate) param_types: Vec<crate::tir::TypeId>,
    /// The receiver is spelled as the first call-site argument — the
    /// trait-qualified (UFCS) shape `Trait::method(recv, …)` — so the
    /// arguments align with the callee's *full* parameter list including
    /// `self`. False for every ordinary static call, whose arguments start
    /// at the first value parameter. Consumers zipping parameters with
    /// arguments (effect parameter resolution) branch on this.
    pub(crate) self_in_args: bool,
}

/// Generic-instantiation decision at a call, struct-literal, or
/// variant-construction site. `type_args` holds the concrete type per generic
/// parameter in declaration order, `instance_type` the resulting
/// `GenericInstance` id or the call's monomorphic target — recorded together so
/// reify drops into the TIR slot without re-running `make_generic_instance`.
#[derive(Clone)]
pub(crate) struct GenericInstantiation {
    pub(crate) type_args: Vec<TypeId>,
    pub(crate) instance_type: TypeId,
    /// Mangled name as the combined walk emitted it onto the TIR node
    /// (`StructLiteral::struct_name`, `Call::FuncRef::name`, ...).
    /// `None` for sites that don't carry a mangled name (e.g. when the
    /// instantiation is recorded purely for type-arg replay).
    ///
    /// Recording it here lets reify drop its own `mangle_generic_name`
    /// reconstruction at struct-literal / call sites — the parity-bug
    /// class WEP 2026-05-26 §"Reify — mechanical" calls out (`type_name(t)`
    /// drift between annotate and reify) goes away by construction.
    pub(crate) mangled_name: Option<String>,
    /// True for an anonymous composition (`{ ..a, ..b }`): reify projects the
    /// union fields from the spread bases instead of the explicit fields alone.
    pub(crate) is_union: bool,
}

/// A single mutating outer-binding captured by a closure. The closure
/// pre-pass at `closure.rs:140–193` materialises a
/// `let __ref_<var_name> = &mut <var_name>;` in the outer scope before
/// opening the closure body; reify replays the same `add_local` at the
/// same point. The fields below carry every value the replay needs.
#[derive(Clone)]
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
    /// `FunctionContext::locals` walk-order invariant); this field is the
    /// cross-check.
    pub(crate) outer_index: u32,
}

/// One entry in the closure's capture list. Mirrors
/// [`crate::tir::TirCapture`] but lives off the TIR so reify produces
/// the same shape from the recorded info.
#[derive(Clone)]
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
pub(crate) struct AssertSlot {
    /// The flagged sub-expression's [`AstId`].
    pub(crate) ast_id: AstId,
    /// The user-facing label the panic template uses for this slot.
    pub(crate) capture_label: String,
}

/// Power-assert capture map recorded by
/// [`super::super::Elaborator::desugar_assert`]. Reify walks the
/// condition AST and consults `slots` to decide which sub-expressions
/// become `let __vK = …;` (slots whose AST evaporated during
/// resolution stay unbound and are skipped by the template).
#[derive(Clone)]
pub(crate) struct AssertCaptureInfo {
    pub(crate) slots: Vec<AssertSlot>,
}

/// Handler-binding resolution facts recorded once per
/// [`crate::ast::EffectHandlerBinding`] at annotate time. Reify
/// reads this entry to enumerate the
/// `TirHandlerBinding`s without re-running
/// `collect_effect_impls_for_type` or the explicit-form
/// `trait_env` validation.
#[derive(Clone)]
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
pub(crate) struct HandlerEffectEntry {
    pub(crate) name: String,
    pub(crate) module_source: crate::module_source::ModuleSource,
    pub(crate) trait_type_args: Vec<TypeId>,
}

/// Impl-block resolution facts recorded once per `impl` block at
/// annotate time. Reify reads this entry
/// keyed by the [`crate::ast::ImplBlock`]'s [`AstId`] and uses
/// every field verbatim — no re-resolution of the impl target,
/// the trait reference, the type params, or the associated
/// types happens inside `reify_impl`.
#[derive(Clone)]
pub(crate) struct ImplFacts {
    /// The trait this block implements, named by the module that declares it
    /// and carrying the header's type arguments (`Stream<u8>`) — `None` for an
    /// inherent impl. Lives on `FunctionRef::method_info`'s `trait_name`.
    ///
    /// The declaration and the instantiated spelling travel together, so two
    /// modules' same-named traits stay apart in the `trait_env` dispatch
    /// indices and in the method mangle alike.
    pub(crate) trait_name: Option<crate::name::FqTraitName>,
    /// Concrete `TypeId`s of the trait/resource type arguments at the
    /// impl site (`impl Future<i32> for …` → `[i32]`; `impl<T> Stream<T>`
    /// → the impl's `TypeParam` id). Written onto each method's
    /// `LocalMethodName::trait_type_args`; the effect-dispatch synthesis
    /// keys its handler index on `(struct, effect_module, base_trait,
    /// trait_type_args)`, so a generic-effect handler needs the args to
    /// match the binding's instantiation.
    pub(crate) trait_type_args: Vec<crate::tir::TypeId>,
    /// True iff the impl's trait reference names an effect
    /// (`interface`) declaration — i.e. this is an effect handler
    /// impl. Reify writes onto `FunctionContext::in_handler_method`
    /// so `resume` validation matches annotate.
    pub(crate) is_handler_method: bool,
    /// True iff the impl target is `&T` / `&mut T` (ref-type
    /// impl). Method receivers `&self` get an extra `&` layer at
    /// receiver-adjustment time; mirrors [`MethodDispatch::is_ref_impl`]
    /// but is decided at impl-block scope.
    pub(crate) is_ref_impl: bool,
    /// Mangled struct name the elaborator feeds into
    /// [`crate::name::MethodName::format_local`] when naming the impl's methods.
    /// Reify reads it verbatim rather than rebuilding it from [`Self::self_type`],
    /// which would mean re-encoding the `&` / `&mut` / tuple cases and the
    /// `&T`-blanket carve-out that `get_type_name` already handles.
    pub(crate) struct_name: crate::name::FqTypeName,
    /// The impl target's typed receiver, decided from the AST type at record
    /// time (`Receiver::Ref` for `&T` / `&mut T`, `Receiver::Type` otherwise).
    /// Reify builds the method's `LocalMethodName` from this so a ref impl's
    /// receiver stays typed end-to-end — no string is re-inspected to recover
    /// the `&` shape. `head_key()` reproduces [`Self::struct_name`].
    pub(crate) receiver: Receiver,
    /// Per-instantiation owner name (`"List<u8>"`) when this impl is a fully
    /// concrete generic instantiation (`impl List<u8>`, `impl Tag for
    /// List<u8>`) — `None` otherwise. Reify names such methods
    /// `List<u8>::method` and emits them as standalone concrete functions
    /// (no impl type params, no monomorphization). Computed AST-side (with
    /// declared impl type params excluded) so it agrees with method dispatch's
    /// `from_concrete_impl`, regardless of how `self_type` resolved a param
    /// that happens to be named like a known type.
    pub(crate) concrete_owner: Option<crate::name::FqTypeName>,
}

/// Operator-dispatch decision recorded when the elaborator lowers a binary
/// or index expression to a trait method call. Reify checks this map before
/// falling back to the native [`crate::tir::TirExprKind::Binary`] /
/// [`crate::tir::TirExprKind::Index`] path.
#[derive(Clone)]
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
    /// `true` when reify must wrap the method call in an outer
    /// `Unary { Deref }` — the `Index` trait returns `&Output`, so
    /// `expr[i]` lowers to `*expr.index(i)`. `IndexValue` and the
    /// arithmetic/comparison operator dispatches return the value
    /// directly and set this `false`. Recorded explicitly because the
    /// return-type shape alone is ambiguous: an `IndexValue` whose
    /// `Output` is itself a reference (`List<&i32>::index_value` →
    /// `&i32`) would otherwise be mistaken for the `Index` shape and
    /// double-dereferenced.
    pub(crate) needs_deref: bool,
}

/// `for x of expr` iterator dispatch result.
/// Recorded only on the iterator path (`DesugarKind::ForOfIterator`);
/// tuple and variadic paths don't dispatch, so they don't record here.
///
/// The two `FunctionRef`s are the same dispatch targets the elaborator
/// resolved through `resolve_method_call_with(method_id: None,
/// call_id: None)`, captured here so reify emits the synthetic calls
/// without re-dispatching.
#[derive(Clone)]
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
/// compiler architecture note in `wado-compiler/CLAUDE.md`); reify reads
/// this tag to pick the same expansion without re-deciding the shape.
#[derive(Clone, Copy, PartialEq, Eq)]
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
    /// `Base::from(Newtype_val)` where `Newtype = Base` — the elaborator
    /// lowers to a `Cast` of the argument to the base type. Reify reads
    /// this tag on the outer `Call`'s `AstId` and emits the same shape.
    NewtypeFromUnwrap,
    /// `Newtype::from(Base_val)` where `Newtype = Base` — the elaborator
    /// lowers to a `Cast` of the argument to the newtype. Reify reads
    /// this tag on the outer `Call`'s `AstId` and emits the same shape.
    NewtypeFromWrap,
}
