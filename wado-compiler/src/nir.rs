//! Normalized Intermediate Representation (NIR) for Wado.
//!
//! NIR is the post-lower body IR consumed by `optimize` and `wir_build`.
//! At the moment NIR lands, its expression / statement / pattern / container
//! shapes are structurally identical to their TIR counterparts; they are
//! distinct Rust types — separate enums, separate visitor — so the type
//! system enforces the "lower has run" precondition.
//!
//! Type identity (`TypeId`, `TypeTable`, `ResolvedType`, …) and effect
//! references (`EffectRef`) are shared with TIR.
//!
//! See `docs/wep-2026-05-11-nir.md` for the full rationale.

use std::cell::RefCell;
use std::rc::Rc;

use crate::hashmap::{IndexMap, IndexSet};

use crate::module_source::ModuleSource;
use crate::name::LocalMethodName;
use crate::tir::{EffectRef, TypeId, TypeTable};
use crate::token::Span;

#[derive(Debug, Clone)]
pub struct NirExpr {
    pub kind: NirExprKind,
    pub type_id: TypeId,
    pub span: Span,
}

impl NirExpr {
    pub fn new(kind: NirExprKind, type_id: TypeId, span: Span) -> Self {
        Self {
            kind,
            type_id,
            span,
        }
    }
}

#[derive(Debug, Clone)]
pub struct FunctionRef {
    pub module_source: ModuleSource,
    pub name: String,
    pub monomorph_info: Option<MonomorphInfo>,
    pub method_info: Option<LocalMethodName>,
}

impl FunctionRef {
    /// Create a `FunctionRef` by extracting metadata from a resolved `NirFunction`.
    pub fn from_resolved(func: &NirFunction, module_source: ModuleSource) -> Self {
        Self {
            module_source,
            name: func.name.clone(),
            monomorph_info: func.monomorph_info.clone(),
            method_info: func.method_info.clone(),
        }
    }

    /// Get the module path (for backwards compatibility)
    pub fn module_path(&self) -> Vec<String> {
        self.module_source.to_path()
    }

    /// Get the fully qualified function name including module path.
    pub fn full_name(&self) -> String {
        if let Some(info) = &self.method_info {
            info.to_mangled_name()
        } else if self.module_source.is_entry_point() {
            self.name.clone()
        } else {
            let path = self.module_source.to_path();
            format!("{}/{}", path.join("/"), &self.name)
        }
    }

    /// Get the builtin function name if this is a builtin call.
    /// Returns the qualified name (e.g., "`builtin::array_len`").
    ///
    /// Functions declared in `core:builtin` and functions synthesised
    /// from wasm-asset exports (`ModuleSource::Wasm`) both go through
    /// the import-style builtin lowering — they share `#[canonical(...)]`
    /// metadata in `BuiltinRegistry` and resolve to the same wasm
    /// import call shape.
    pub fn builtin_name(&self) -> Option<String> {
        if self.monomorph_info.is_some() {
            return None;
        }
        if self.module_source.is_core_builtin() || self.module_source.is_wasm_asset() {
            Some(format!("builtin::{}", &self.name))
        } else {
            None
        }
    }

    /// True when this reference resolves to one of the `builtin::likely`
    /// / `builtin::unlikely` branch-hint intrinsics. Used by
    /// optimization passes that need to peer through the wrapper to
    /// reason about the underlying condition.
    ///
    /// Funnelled through [`Self::builtin_name`] so the inclusion
    /// criterion (non-monomorphized, source is `core:builtin` or a
    /// wasm asset) stays in sync with the WIR builder's
    /// `BranchHint` lowering at `wir_build/calls.rs`.
    #[must_use]
    pub fn is_branch_hint_call(&self) -> bool {
        matches!(
            self.builtin_name().as_deref(),
            Some("builtin::likely" | "builtin::unlikely")
        )
    }

    /// Get the monomorphized builtin name if this is a monomorphized builtin function.
    pub fn monomorphized_builtin_name(&self) -> Option<String> {
        let generic_name = self
            .monomorph_info
            .as_ref()
            .map(|i| i.generic_name.as_str())?;

        match generic_name {
            "array_get"
            | "array_set"
            | "array_new"
            | "array_len"
            | "array_copy"
            | "array_fill"
            | "array_clone"
            | "array_clone_shallow"
            | "select"
            | "copy_value" => Some(format!("builtin::{generic_name}")),
            _ => None,
        }
    }

    /// Check if this function is monomorphized (instantiated from a generic)
    pub fn is_monomorphized(&self) -> bool {
        self.monomorph_info.is_some()
    }

    /// Get the base generic name if this is a monomorphized function.
    pub fn base_struct_name(&self) -> Option<String> {
        self.monomorph_info
            .as_ref()
            .and_then(|info| info.generic_name.split("::").next())
            .map(std::string::ToString::to_string)
    }

    /// Check if this is a method (instance or static) as opposed to a free function.
    pub fn is_method(&self) -> bool {
        self.method_info.is_some()
    }

    /// Check if this is a trait method.
    pub fn is_trait_method(&self) -> bool {
        self.method_info
            .as_ref()
            .is_some_and(LocalMethodName::is_trait_method)
    }
}

/// A function argument bundled with its parameter mutability metadata.
///
/// `is_mut` reflects whether the callee declares this parameter as `mut`.
/// It controls value-copy semantics at the call site in the WIR translation phase.
/// An empty `args` list or missing metadata defaults to conservative (copy).
#[derive(Debug, Clone)]
pub struct CallArg {
    pub expr: NirExpr,
    /// Whether the callee declares this parameter as `mut`.
    pub is_mut: bool,
}

impl CallArg {
    pub fn new(expr: NirExpr, is_mut: bool) -> Self {
        Self { expr, is_mut }
    }
}

/// Zero-sized witness that a [`NirExprKind::MethodCall`] was constructed
/// through [`NirExprKind::method_call`], the sole constructor.  The inner
/// `()` is private to this module so no code outside `tir` can build one —
/// this makes direct struct-literal construction of `NirExprKind::MethodCall`
/// impossible and channels every elaborator-side emission through the
/// single checkpoint maintained in `Elaborator::build_tir_method_call`,
/// which in turn guarantees arguments were typechecked against the
/// callee's declared parameter types.
///
/// Post-resolve phases (monomorphize / lower / optimize / codegen) still
/// rebuild `MethodCall` nodes legitimately; they do so via the same
/// constructor, which is pub(crate) and therefore available only inside
/// the compiler.
#[derive(Debug, Clone)]
pub struct MethodCallInvariant(());

impl NirExprKind {
    /// Sole constructor of [`NirExprKind::MethodCall`].
    ///
    /// Callers are expected to have typechecked `args` against the callee's
    /// declared parameter types before reaching here.  Elaborator-side
    /// constructions flow through `Elaborator::build_tir_method_call`;
    /// post-resolve rewriters thread already-checked NIR through this
    /// function too so the variant's `invariant` field stays coherent.
    pub(crate) fn method_call(
        receiver: Box<NirExpr>,
        func: FunctionRef,
        type_args: Vec<TypeId>,
        args: Vec<CallArg>,
    ) -> Self {
        Self::MethodCall {
            receiver,
            func,
            type_args,
            args,
            invariant: MethodCallInvariant(()),
        }
    }
}

#[derive(Debug, Clone)]
pub enum NirExprKind {
    IntLiteral {
        value: u64,
        repr: String,
    },
    FloatLiteral {
        value: f64,
        repr: String,
    },
    BoolLiteral(bool),
    CharLiteral(char),
    StringLiteral(String),
    /// Byte array literal from `#include_bytes`. Lowered to `List<u8>` via data segment.
    BytesLiteral(Vec<u8>),
    Null,
    Unit,

    Local {
        index: u32,
        name: String,
    },
    /// Read a global variable
    GlobalVarGet {
        module_source: ModuleSource,
        name: String,
    },
    /// Write to a global variable
    GlobalVarSet {
        module_source: ModuleSource,
        name: String,
        value: Box<NirExpr>,
    },

    Binary {
        left: Box<NirExpr>,
        op: NirBinaryOp,
        right: Box<NirExpr>,
    },
    Unary {
        op: NirUnaryOp,
        expr: Box<NirExpr>,
    },
    Assign {
        target: Box<NirExpr>,
        value: Box<NirExpr>,
    },
    Cast {
        expr: Box<NirExpr>,
        target_type: TypeId,
    },

    /// Free function call (`foo(args)`) or static method call (`Type::method(args)`).
    ///
    /// Whether this is a static method call is encoded in `func.method_info`:
    /// `None` → free function, `Some(_)` → static method.
    /// Both map to the same `WirInstr::Call` and share identical semantics.
    Call {
        /// Function reference (resolved NIR function or external)
        func: FunctionRef,
        /// Explicit type arguments for generic functions: `identity::<i32>(x)`
        type_args: Vec<TypeId>,
        args: Vec<CallArg>,
    },
    /// Raw Component Model call to a lowered WASI import.
    ///
    /// Used inside synthesized CM binding functions to call the flat-ABI WASI function
    /// directly, bypassing the normal effect call mechanism. Args are already lowered
    /// to flat CM types (i32, i64, f32, f64).
    CmRawCall {
        /// Full WASI local alias name (e.g., "wasi:cli/stdout@0.3.0/write-via-stream")
        local_name: String,
        /// Flat ABI arguments (already lowered to core Wasm types)
        args: Vec<NirExpr>,
    },
    MethodCall {
        receiver: Box<NirExpr>,
        /// Method reference (resolved NIR function or external)
        func: FunctionRef,
        /// Explicit type arguments for generic methods: `obj.method::<i32>()`
        type_args: Vec<TypeId>,
        args: Vec<CallArg>,
        /// Witness that this node was constructed through
        /// [`NirExprKind::method_call`], which is the sole public
        /// constructor.  Its inner field is private to this module, so
        /// code outside `tir` cannot build a `MethodCall` struct literal
        /// directly and must route through the constructor.  Pattern
        /// matches elsewhere in the crate use `..` to ignore this field.
        invariant: MethodCallInvariant,
    },

    FieldAccess {
        expr: Box<NirExpr>,
        field_index: u32,
        field_name: String,
    },
    Index {
        expr: Box<NirExpr>,
        index: Box<NirExpr>,
    },

    Block(NirBlock),
    If {
        condition: Box<NirExpr>,
        then_branch: NirBlock,
        else_branch: Option<NirBlock>,
    },
    Match {
        expr: Box<NirExpr>,
        arms: Vec<NirMatchArm>,
    },

    StructLiteral {
        struct_type: TypeId,
        struct_name: String,
        fields: Vec<NirStructField>,
    },
    TupleLiteral {
        elements: Vec<NirExpr>,
    },
    /// A fixed-length `List<T>` value materialized by
    /// `optimize::array_literal` from an inlined `SequenceLiteralBuilder`
    /// push sequence. `lower` never emits this; it is an
    /// optimizer-materialized normalization, like `Switch`. The `List<T>`
    /// struct type is carried by the enclosing `NirExpr::type_id`, exactly
    /// as `TupleLiteral` relies on `type_id` for the tuple's struct type.
    /// `wir_build` lowers it to the `List<T>` `{ repr, used }` `StructNew`
    /// whose `repr` field is a `WirInstr::ArrayNewFixed` of the elements.
    ArrayLiteral {
        elements: Vec<NirExpr>,
    },

    /// Indirect call through a callable value (closure or funcref)
    IndirectCall {
        /// The callee expression (closure struct or funcref)
        callee: Box<NirExpr>,
        /// Arguments to pass to the callee
        args: Vec<NirExpr>,
    },

    /// Convert a functor struct to canonical closure representation.
    /// Generated by lower phase for closures that need fn-type compatibility
    /// but weren't handled by fn-param specialization.
    ClosureToCanonical {
        /// The functor struct expression (`__Closure_N` literal)
        functor: Box<NirExpr>,
        /// Functor ID for looking up the `__call` method
        functor_id: u32,
        /// Target function type (for canonical closure type lookup)
        target_fn_type: TypeId,
        /// Module where the closure was defined (needed for cross-module DCE)
        closure_module: ModuleSource,
    },

    /// Custom variant construction: `Shape::Circle(5.0)` or `MyVariant::Unit`
    VariantConstruct {
        /// The variant type (e.g., `ResolvedType::Variant` { name: "Shape", ... })
        variant_type: TypeId,
        /// The case index (0-based position in variant declaration)
        case_index: u32,
        /// The case name (for debugging/error messages)
        case_name: String,
        /// Payload value (None for unit variants constructed without explicit payload)
        payload: Option<Box<NirExpr>>,
    },

    /// Enum construction: `Color::Red`
    /// Enums have no payload, just a discriminant value.
    EnumConstruct {
        /// The enum type (e.g., `ResolvedType::Enum` { name: "Color", ... })
        enum_type: TypeId,
        /// The case index (0-based position in enum declaration)
        case_index: u32,
        /// The case name (for debugging/error messages)
        case_name: String,
    },

    /// Labeled block expression: `label: { ... }` that produces a value
    /// The value must be returned via `break label: expr;`
    LabeledBlock {
        label: String,
        block: NirBlock,
        /// The type of value this block produces (from break expressions)
        result_type: TypeId,
    },

    /// Get the discriminant (tag) of a variant value.
    /// Generated from match expressions on variants.
    /// Result type is i32.
    VariantTag {
        expr: Box<NirExpr>,
    },

    /// Test if a variant value is of a specific case.
    /// Generated from if-let patterns on custom variants.
    /// For unit variants: checks discriminator == `case_index`
    /// For payload variants: uses ref.test on the case type
    /// Result type is bool.
    VariantTest {
        expr: Box<NirExpr>,
        /// The case index to test for
        case_index: u32,
        /// The case name (for error messages)
        case_name: String,
    },

    /// Extract the payload from a variant value at a specific case index.
    /// Generated from match expressions on variants.
    VariantPayload {
        expr: Box<NirExpr>,
        /// The case index to extract payload from
        case_index: u32,
        /// The payload type for this case
        payload_type: TypeId,
    },

    /// Switch expression for O(1) dispatch using `br_table`.
    /// Generated by lower phase for dense integer match expressions.
    /// Each arm index corresponds to (`scrutinee_value` - `min_value`).
    Switch {
        /// The scrutinee expression (must be integer type)
        scrutinee: Box<NirExpr>,
        /// Minimum value in the switch range
        min_value: i64,
        /// Arms in order: arm[i] handles value (`min_value` + i)
        /// Length determines the range: `[min_value, min_value + arms.len() - 1]`
        arms: Vec<NirBlock>,
        /// Default arm for values outside the range
        default: NirBlock,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NirBinaryOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    And,
    Or,
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
    RefEq,
    RefNotEq,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NirUnaryOp {
    Neg,
    Not,
    BitNot,
    Ref,
    MutRef,
    Deref,
}

#[derive(Debug, Clone)]
pub struct NirMatchArm {
    pub pattern: NirPattern,
    /// Optional guard expression (the condition after `&&`)
    pub guard: Option<NirExpr>,
    pub body: NirExpr,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum NirPattern {
    Wildcard,
    Binding {
        name: String,
        local_index: u32,
        type_id: TypeId,
    },
    Literal(NirLiteralPattern),
    Tuple(Vec<NirPattern>, /* has_rest */ bool),
    Variant {
        enum_type: TypeId,
        variant_name: String,
        bindings: Vec<NirPattern>,
        /// Payload type for the matched variant case (unit for no-payload cases)
        payload_type: TypeId,
    },
    /// Enum case pattern (enums are simple i32 discriminants with no payload)
    Enum {
        enum_type: TypeId,
        case_name: String,
        case_index: u32,
    },
    /// Struct destructuring pattern: `{ x, y }` or `Point { x, y }`
    Struct {
        struct_type: TypeId,
        fields: Vec<NirStructPatternField>,
        has_rest: bool,
    },
    /// Or pattern: matches if any alternative matches
    Or(Vec<NirPattern>),
    /// Constant value pattern: compares scrutinee against a constant expression
    /// (immutable global variable or associated constant like `i32::MAX`)
    ConstantValue {
        expr: Box<NirExpr>,
    },
    /// Range pattern: `0..<10` or `'a'..='z'`
    Range {
        start: i128,
        end: i128,
        inclusive: bool,
        is_unsigned: bool,
    },
}

#[derive(Debug, Clone)]
pub struct NirStructPatternField {
    pub field_name: String,
    pub field_index: u32,
    pub pattern: NirPattern,
}

#[derive(Debug, Clone)]
pub enum NirLiteralPattern {
    /// Signed integer literal (covers i8, i16, i32, i64, i128)
    I128(i128),
    /// Unsigned integer literal (covers u8, u16, u32, u64, u128)
    U128(u128),
    Bool(bool),
    Char(char),
    String(String),
    Null,
}

#[derive(Debug, Clone)]
pub struct NirStructField {
    pub name: String,
    pub value: NirExpr,
    pub field_index: u32,
}

#[derive(Debug, Clone)]
pub struct NirCapture {
    pub name: String,
    pub outer_index: u32,
    pub type_id: TypeId,
    pub is_mut: bool,
}

#[derive(Debug, Clone)]
pub struct NirBlock {
    pub stmts: Vec<NirStmt>,
    pub span: Span,
}

impl NirBlock {
    pub fn new(stmts: Vec<NirStmt>, span: Span) -> Self {
        Self { stmts, span }
    }

    pub fn empty(span: Span) -> Self {
        Self {
            stmts: Vec::new(),
            span,
        }
    }
}

#[derive(Debug, Clone)]
pub struct NirStmt {
    pub kind: NirStmtKind,
    pub span: Span,
}

impl NirStmt {
    pub fn new(kind: NirStmtKind, span: Span) -> Self {
        Self { kind, span }
    }
}

#[derive(Debug, Clone)]
pub enum NirStmtKind {
    Let {
        name: String,
        local_index: u32,
        is_mut: bool,
        is_reactive: bool,
        type_id: TypeId,
        value: NirExpr,
        /// When true, the WIR builder skips deep value-copy for this binding.
        /// Set by LICM for hoisted variables whose source field is verified
        /// non-mutated in the loop, making aliasing safe.
        skip_value_copy: bool,
    },
    Expr(NirExpr),
    Return {
        value: Option<NirExpr>,
    },
    If {
        condition: NirExpr,
        then_block: NirBlock,
        else_block: Option<NirBlock>,
    },
    /// Canonical loop: `loop { ... }` - infinite loop exited via break
    Loop {
        body: NirBlock,
    },
    /// Break statement: `break;`, `break label;`, or `break label: expr;`
    Break {
        /// Optional label to break to (for labeled blocks)
        label: Option<String>,
        /// Optional value to return from the labeled block
        value: Option<NirExpr>,
    },
    Continue,
    /// Labeled block: `LABEL: { ... }` - creates a new scope with local bindings
    LabeledBlock {
        label: String,
        block: NirBlock,
    },
    /// Tuple destructuring let statement: `let [a, b] = tuple_expr;`
    ///
    /// Preserved post-lower only when `lower::translate::pattern` detects a tuple-shaped
    /// pattern whose value is a core-builtin Call (multi-value return).
    /// Codegen has a special optimization for that case; all other shapes are
    /// rewritten by `lower::translate::pattern` before reaching NIR.
    LetDestructure {
        /// The pattern to bind (e.g., [a, b, c] or [x, [y, z]])
        pattern: NirPattern,
        /// Whether bindings are mutable
        is_mut: bool,
        /// The value expression (must be a tuple)
        value: NirExpr,
    },
}

/// Generic type parameter in NIR (from AST `GenericParam`)
#[derive(Debug, Clone)]
pub struct NirTypeParam {
    pub name: String,
    /// Whether this is an effect parameter (`effect E`)
    pub is_effect: bool,
    /// Whether this is a type pack parameter (`..T`)
    pub is_pack: bool,
    pub bounds: Vec<String>,
    /// Default type if specified (e.g., `Effects = []`)
    pub default: Option<TypeId>,
    pub index: u32,
}

/// Information about monomorphization origin for instantiated items
#[derive(Debug, Clone)]
pub struct MonomorphInfo {
    /// Original generic name (e.g., "Box" for "Box<i32>", or "`BTreeNode`<`K,V>::insert`" for methods)
    pub generic_name: String,
    /// Impl-level type arguments (from the struct/type, e.g. `[i32]` for `List<i32>`)
    pub impl_type_args: Vec<TypeId>,
    /// Method-level type arguments (from the method's own generics, e.g. `[String]` for `.transform::<String>()`)
    pub method_type_args: Vec<TypeId>,
    /// Whether this originates from a blanket impl (e.g., `impl<I: Iterator> IntoIterator for I`)
    pub is_blanket: bool,
}

/// Global variable declaration in NIR
#[derive(Debug, Clone)]
pub struct NirGlobal {
    pub name: String,
    pub ty: TypeId,
    pub initializer: NirExpr,
    pub mutable: bool,
    /// Whether the user declared this global as `global mut`.
    /// Preserved across lowering so the optimizer can promote lazy-init globals
    /// back to immutable when their initializers fold to constants.
    pub wado_mutable: bool,
    pub is_pub: bool,
    /// Module where this global is defined
    pub module_source: ModuleSource,
    pub span: Span,
    /// True if this global's Wasm type should be nullable.
    /// Set by the lower phase for two distinct cases:
    /// 1. Lazy-initialized reference globals — the slot starts `null`
    ///    until `__initialize_module` runs, so the storage must accept
    ///    `ref.null`. (`lazy_init` is also set in this case.)
    /// 2. Constant-initialized reference globals whose user-facing
    ///    initializer is `null` (e.g. `global mut x: Option<&T> = null`)
    ///    — the slot needs to accept `ref.null` because that IS the
    ///    intended runtime value. (`lazy_init` stays false.)
    pub is_nullable: bool,
    /// True when this global is lazy-initialized: the Wasm slot starts
    /// `null`, and `__initialize_module` runs the original (non-constant)
    /// initializer to assign the real value before any non-init use.
    /// Codegen narrows `global.get` results with `ref.as_non_null` for
    /// these globals, since the read result is guaranteed non-null after
    /// init.
    ///
    /// `false` for constant-initialized globals (including
    /// `Option<&T> = null` whose `null` is itself the runtime value) —
    /// codegen leaves the read result nullable so a `None` value reads
    /// back as `ref.null` instead of trapping in `ref.as_non_null`.
    pub lazy_init: bool,
    /// Per-local metadata for the initializer expression. Populated when
    /// the initializer is non-trivial (e.g., `SequenceLiteralBuilder`
    /// coercion). Indexed by local index, like `NirFunction::locals`.
    pub locals: Vec<NirLocal>,
}

#[derive(Debug, Clone)]
pub struct NirFunction {
    pub name: String,
    /// Module this function belongs to. Set by the link phase when flattening
    /// per-module body data into flat lists; before link, the `module_source` is
    /// carried implicitly by the parent `NirModule`.
    pub module_source: ModuleSource,
    pub is_pub: bool,
    /// Whether this function is exported at the Component Model boundary (world export)
    pub is_export: bool,
    /// Whether this is an async function (`export async fn`).
    /// Async functions use `task return` instead of `return` to deliver results.
    pub is_async: bool,
    /// Generic type parameters (empty for non-generic functions)
    pub type_params: Vec<NirTypeParam>,
    /// Type parameters from the impl block (for methods on generic structs)
    /// e.g., for a method in `impl Counter<T>`, this contains T's info
    pub impl_type_params: Vec<NirTypeParam>,
    /// If this function was created by monomorphization, contains the origin info
    pub monomorph_info: Option<MonomorphInfo>,
    /// Parsed method info for methods (None for free functions)
    /// Contains `struct_name`, `trait_name`, and `method_name` extracted from the function name.
    pub method_info: Option<LocalMethodName>,
    pub params: Vec<NirParam>,
    pub return_type: TypeId,
    /// Declared return type for `async fn` (where `return_type` is erased to unit
    /// because the result is delivered via `task return`). `None` for non-async fns
    /// and for CM-binding / synthesized wrappers. Preserved so the effect checker
    /// can infer signature resources from the user-visible return type.
    pub task_return_type: Option<TypeId>,
    pub effects: Vec<EffectRef>,
    /// Parameter names declared in `stores[...]` — the function may store these references.
    pub stores: Vec<String>,
    pub body: Option<crate::nir_arena::Body>,
    pub span: Span,
    pub local_count: u32,
    /// Per-local metadata — `name`, `type_id`, `is_mut` — indexed by Wasm
    /// local index. Entries `0..params.len()` shadow the corresponding
    /// `params[i]` (for uniform absolute indexing); body let-bindings and
    /// elaborator/optimizer-allocated temporaries occupy `params.len()..`.
    /// `local_count == locals.len()` post-resolve; passes that grow the
    /// local set must keep the two in sync.
    pub locals: Vec<NirLocal>,
    /// Local indices that have their address taken (&x or &mut x).
    /// For mutable primitives, these locals are stored in box structs.
    pub address_taken_locals: IndexSet<u32>,

    /// Local indices whose references were stored by inlined `stores` functions.
    /// When inlining `fn f(x: &T) with stores[x]` with argument `&local`,
    /// `local` is added here to prevent SROA from decomposing it.
    pub stores_aliased_locals: IndexSet<u32>,

    /// Whether this function is a synthesized CM binding (generated by `synthesis::cm_binding`).
    /// The inliner and effect checker both skip CM bindings because they are ABI bridges
    /// between Wado GC types and CM linear memory with special effect semantics.
    pub is_cm_binding: bool,

    /// Whether this function is a synthesised effect-dispatch wrapper
    /// (generated by `synthesis::effect_dispatch`). Effect-operation
    /// call-site rewriting must skip these — their fallback path
    /// directly calls `__cm_binding__<E>_<op>`, which would loop back
    /// through the wrapper if rewritten.
    pub is_dispatch_wrapper: bool,

    /// Whether this function is a synthesized CM *export* binding (world export wrapper).
    /// When true, the global initializer (`__initialize_modules`) is injected at the start
    /// of this function's body during lowering.
    pub is_cm_export: bool,

    /// Whether this function is marked `#[ambient]`. Ambient functions are implicitly
    /// available to callers without requiring matching `with` clauses — they still carry
    /// interface declarations for documentation / implementation purposes, but the effect
    /// checker does not propagate those requirements to callers.
    pub is_ambient: bool,

    /// Inline hint from `#[inline]`, `#[inline(always)]`, or `#[inline(never)]` attributes.
    pub inline_hint: InlineHint,

    /// The compiler-recognized stdlib role this function fills, if any.
    /// Set from `#[compiler_item("...")]` on the source declaration; see
    /// [`crate::compiler_item::CompilerItem`].
    pub compiler_item: Option<crate::compiler_item::CompilerItem>,

    /// Custom wasm export name from `#[export_name("...")]` attribute.
    pub export_name: Option<String>,

    /// Allocator tag from `#[allocator("...")]` attribute (e.g., `"bump"`, `"debug"`).
    pub allocator_tag: Option<String>,

    /// Categorizes the function for kind-specific optimizations. Most functions
    /// are `Regular`; synthesis passes set specialized kinds so the NIR
    /// optimizer can apply targeted transformations (e.g. freshness-based
    /// elision for `ValueCopy`).
    pub kind: FunctionKind,

    /// ABI for delivering the function's return value at WIR / Wasm level.
    /// Defaults to [`ReturnAbi::Single`]; an analysis pass sets
    /// [`ReturnAbi::MultiValue`] for tuple- or user-struct-returning
    /// functions whose every call site destructures the result via
    /// `FieldAccess` and whose body's returns produce a fresh
    /// `TupleLiteral` / `StructLiteral`. WIR build then emits a
    /// multi-value Wasm result signature (no heap struct round-trip).
    pub return_abi: ReturnAbi,
}

/// How a function delivers its return value at the Wasm level.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ReturnAbi {
    /// Single Wasm return value. The function's NIR `return_type` is taken
    /// as-is; tuple / user-struct types lower to a heap struct ref.
    #[default]
    Single,
    /// Multi-value Wasm return: each tuple element / struct field becomes a
    /// separate Wasm result. Carries the per-element NIR type ids and field
    /// names for WIR-build's signature emission and call-site split-local
    /// generation. The function's NIR `return_type` is unchanged (it remains
    /// the tuple / struct type) — only the WIR-level ABI shifts.
    ///
    /// For tuple returns, `field_names` is `["0", "1", ...]` (matching the
    /// numeric field names tuple structs carry). For user-struct returns,
    /// `field_names` is the struct's fields in declaration order.
    MultiValue {
        /// NIR types of each result, in declaration order.
        result_types: Vec<TypeId>,
        /// Field names matching the source aggregate's declaration order.
        /// Used by WIR build to look up the right split local from a
        /// `FieldAccess` access on a multi-value-bound temp.
        field_names: Vec<String>,
    },
}

/// Semantic category of a `NirFunction`. Carries the type operand so the
/// optimizer can reason about the call without re-deriving it from the
/// signature.
/// Identifies which `Fn<N, Ret>` trait method an auto-derived
/// dispatch stub implements. Recovered from
/// [`FunctionKind::FnCanonicalDispatch`] so WIR build can choose
/// the right vtable slot (`inspect` vs `inspect_alt`) without
/// re-parsing mangled names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FnDispatchTrait {
    Inspect,
    InspectAlt,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum FunctionKind {
    /// Ordinary user-defined or synthesized function.
    #[default]
    Regular,
    /// Synthesized `copy_value` function that deep-copies a value of
    /// `type_id`. Calls to such functions may be elided when the argument is
    /// provably fresh.
    ValueCopy { type_id: TypeId },
    /// Auto-derived `Fn<arity, return_type>^Inspect::inspect` (or
    /// `^InspectAlt::inspect_alt`) dispatch stub.
    ///
    /// The NIR body is `unreachable()` — a placeholder that exists
    /// only so the function is registered and the call is resolvable
    /// from templates and from user code. WIR build recognises this
    /// kind and supplies the real body: a `call_ref` through the
    /// matching `CanonicalClosure_K`'s `inspect` / `inspect_alt`
    /// vtable slot. Carries `(arity, return_type)` as structured
    /// fields so neither WIR build nor DCE has to recover them by
    /// parsing the mangled function name.
    ///
    /// See WEP: Inspect (Debug Output) > Closure Inspect via Runtime
    /// Dispatch.
    FnCanonicalDispatch {
        trait_kind: FnDispatchTrait,
        arity: usize,
        return_type: TypeId,
    },
}

/// Inline hint for a function, extracted from `#[inline(...)]` attributes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InlineHint {
    /// No hint — the optimizer decides based on heuristics.
    #[default]
    Auto,
    /// `#[inline]` — suggest inlining (raises the threshold).
    Hint,
    /// `#[inline(always)]` — always inline regardless of size.
    Always,
    /// `#[inline(never)]` — never inline.
    Never,
}

impl NirFunction {
    /// Read-only tree view of the function body (`Body` → `NirBlock`). The
    /// optimizer operates on the arena `Body` directly; the remaining callers
    /// are the diagnostics that still walk a tree (`nir_unparse`, `remarks`).
    pub fn body_block(&self) -> Option<NirBlock> {
        self.body.as_ref().map(crate::nir_arena::Body::to_block)
    }

    /// Build the function body from a tree (`NirBlock` → `Body`). Counterpart
    /// of [`Self::body_block`]; used by tests and tree-shaped body builders to
    /// install an arena body from a constructed `NirBlock`.
    pub fn set_body_block(&mut self, block: NirBlock) {
        self.body = Some(crate::nir_arena::Body::from_block(&block));
    }

    /// Returns true if this is a method (belongs to a struct)
    #[inline]
    pub fn is_method(&self) -> bool {
        self.method_info.is_some()
    }

    /// Returns true if this is a trait method (implements a trait)
    #[inline]
    pub fn is_trait_method(&self) -> bool {
        self.method_info
            .as_ref()
            .is_some_and(super::name::LocalMethodName::is_trait_method)
    }

    /// Returns true if this is the synthesized `__call` method on a
    /// `__Closure_N` functor struct. See
    /// [`LocalMethodName::is_closure_call`] for the rationale.
    #[inline]
    pub fn is_closure_call(&self) -> bool {
        self.method_info
            .as_ref()
            .is_some_and(super::name::LocalMethodName::is_closure_call)
    }

    /// Returns true if this function has type params that need monomorphization
    /// (excludes effect params, which are erased at compile time).
    #[inline]
    pub fn has_real_type_params(&self) -> bool {
        self.type_params.iter().any(|p| !p.is_effect)
    }

    /// Returns the copied type if this is a synthesized value-copy function.
    #[inline]
    pub fn value_copy_type(&self) -> Option<TypeId> {
        match self.kind {
            FunctionKind::ValueCopy { type_id } => Some(type_id),
            _ => None,
        }
    }

    /// Returns the dispatch coordinates if this is an auto-derived
    /// `Fn<arity, return_type>^Inspect` / `^InspectAlt` stub.
    /// WIR build uses the result to supply the indirect-call body
    /// without scanning mangled function names.
    #[inline]
    pub fn fn_canonical_dispatch(&self) -> Option<(FnDispatchTrait, usize, TypeId)> {
        match self.kind {
            FunctionKind::FnCanonicalDispatch {
                trait_kind,
                arity,
                return_type,
            } => Some((trait_kind, arity, return_type)),
            _ => None,
        }
    }

    /// Returns true if this function was synthesized as a value-copy helper.
    #[inline]
    pub fn is_value_copy(&self) -> bool {
        matches!(self.kind, FunctionKind::ValueCopy { .. })
    }
}

/// A resolved local-slot entry in a function, global initializer, or
/// closure scope, identified by its declaration / order in the surrounding
/// local environment.
///
/// `FunctionContext::add_local` records every local — source-level
/// parameters, `let` bindings, destructure bindings, and elaborator-generated
/// temporaries — as a `NirLocal`. The single source of truth for the local
/// namespace is `FunctionContext::locals: Vec<NirLocal>`; from there it is
/// projected onto:
///
/// * `NirFunction::locals` and `NirGlobal::locals` — the function/global's
///   absolute local table, keyed by Wasm local index.
/// * `NirExprKind::Closure { body_locals, .. }` — the closure's
///   body-level let-bindings (params live in `params` so they aren't
///   duplicated). Pattern lowering reconstructs the closure-scope local
///   table from `params + body_locals` while descending in.
#[derive(Debug, Clone)]
pub struct NirLocal {
    /// Source-level name of the binding (or a synthesised `__name` for
    /// elaborator-generated temporaries that have no surface syntax).
    pub name: String,
    pub type_id: TypeId,
    pub is_mut: bool,
}

impl NirLocal {
    /// Build a `NirLocal` for a synthesised slot whose name follows the
    /// `__local_N` convention used by `wir_build` when no source-level
    /// name is available.
    pub fn synth(index: u32, type_id: TypeId, is_mut: bool) -> Self {
        Self {
            name: format!("__local_{index}"),
            type_id,
            is_mut,
        }
    }
}

#[derive(Debug, Clone)]
pub struct NirParam {
    pub name: String,
    pub type_id: TypeId,
    pub local_index: u32,
    pub is_mut: bool,
    /// Resolved default expression for trailing parameters with `= expr`.
    /// Used by the elaborator to synthesize arguments at call sites.
    pub default_expr: Option<Box<NirExpr>>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct NirStruct {
    pub name: String,
    pub module_source: ModuleSource,
    pub is_pub: bool,
    /// Generic type parameters (empty for non-generic structs)
    pub type_params: Vec<NirTypeParam>,
    /// If this struct was created by monomorphization, contains the origin info
    pub monomorph_info: Option<MonomorphInfo>,
    pub fields: Vec<NirField>,
    pub span: Span,
    /// `#[serde(rename_all = "...")]` — naming strategy for all fields.
    pub serde_rename_all: Option<String>,
}

#[derive(Debug, Clone)]
pub struct NirField {
    pub name: String,
    pub is_pub: bool,
    pub type_id: TypeId,
    pub index: u32,
    pub span: Span,
    /// `#[hidden]` — field not shown in debug inspect output.
    pub is_hidden: bool,
    /// `#[serde(rename = "name")]` — custom serialization name for this field.
    pub serde_rename: Option<String>,
    /// `#[serde(default)]` — use default value when field is missing during deserialization.
    pub serde_default: bool,
    /// Resolved default expression for `struct S { x: T = expr }`.
    /// Inserted by the elaborator when the field is omitted in a struct literal.
    pub default_expr: Option<Box<NirExpr>>,
}

#[derive(Debug, Clone)]
pub struct NirEnum {
    pub name: String,
    pub module_source: ModuleSource,
    pub is_pub: bool,
    /// Generic type parameters (empty for non-generic enums)
    pub type_params: Vec<NirTypeParam>,
    /// If this enum was created by monomorphization, contains the origin info
    pub monomorph_info: Option<MonomorphInfo>,
    pub cases: Vec<NirEnumCase>,
    pub span: Span,
}

/// A case in an NIR enum.
/// Unlike `NirVariantCase`, enum cases have no payload.
#[derive(Debug, Clone)]
pub struct NirEnumCase {
    pub name: String,
    pub index: u32,
    pub span: Span,
}

/// A flags type declaration (bitmask type, like WIT flags)
/// e.g., `flags PathFlags { SymlinkFollow }`
/// Represented as `ResolvedType::Flags`; each member is a bitmask value (1 << index).
#[derive(Debug, Clone)]
pub struct NirFlags {
    pub name: String,
    pub module_source: ModuleSource,
    pub is_pub: bool,
    /// The newtype `TypeId` (base type is u32)
    pub type_id: TypeId,
    pub members: Vec<NirFlagsMember>,
    pub span: Span,
}

/// A member of a flags type
#[derive(Debug, Clone)]
pub struct NirFlagsMember {
    pub name: String,
    /// Bitmask value: `1 << index`
    pub bitmask: u32,
    pub span: Span,
}

/// A variant type declaration (tagged union, distinct from enum)
/// e.g., `variant Shape { Circle(f64), Rectangle(f64, f64), Point }`
#[derive(Debug, Clone)]
pub struct NirVariantDecl {
    pub name: String,
    pub module_source: ModuleSource,
    pub is_pub: bool,
    /// Generic type parameters (e.g., `T` in `variant Option<T>`)
    pub type_params: Vec<NirTypeParam>,
    /// Cases of the variant (e.g., Some, None for Option)
    pub cases: Vec<NirVariantCase>,
    pub span: Span,
}

/// A case in a variant declaration
/// e.g., `Circle(f64)` or `Point`
///
/// Each variant case has exactly one payload type:
/// - Unit variants: `None` → payload is `()` (unit type)
/// - Scalar payloads: `Some(T)` → payload is `T`
/// - Tuple payloads: `Rectangle([f64, f64])` → payload is `[f64, f64]`
#[derive(Debug, Clone)]
pub struct NirVariantCase {
    pub name: String,
    /// Case index (0-based)
    pub index: u32,
    /// Payload type for this case. Unit variants have `()` (unit type) payload.
    pub payload: TypeId,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct NirNewtype {
    pub name: String,
    pub module_source: ModuleSource,
    pub is_pub: bool,
    pub type_id: TypeId,
    pub span: Span,
}

/// Test declaration metadata
/// The actual test code is stored as a `NirFunction` in the functions list.
#[derive(Debug, Clone)]
pub struct NirTest {
    /// The original test name from source (None if unnamed)
    pub name: Option<String>,
    /// Generated function name (e.g., "__`test_0`", "__`test_trap_0`", or "__`test_todo_0`")
    pub function_name: String,
    /// Source line number for unnamed test identification
    pub line: usize,
    pub span: Span,
    /// Whether this test is expected to trap (from `#[expect_trap]` attribute)
    pub expect_trap: bool,
    /// Whether this test is a TODO placeholder (from `#[TODO]` attribute).
    /// Like `expect_trap`, the test passes when the body traps, but the runner emits
    /// a distinct message when the body unexpectedly passes, reminding the developer
    /// to remove the `#[TODO]` attribute.
    pub is_todo: bool,
    /// Per-test timeout in milliseconds (from `#[timeout_ms(N)]` attribute).
    /// `None` means use the default timeout (1 second).
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct NirEffect {
    pub name: String,
    pub is_pub: bool,
    pub operations: Vec<NirEffectOp>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct NirEffectOp {
    pub name: String,
    pub params: Vec<NirParam>,
    pub return_type: TypeId,
    pub span: Span,
    /// CM canonical name from `#[cm("...")]` on the resource method
    /// declaration (e.g. `"stream-write"`, `"future-read"`). `None` for
    /// effect operations and for resource methods that don't carry a
    /// CM attribute. The dispatch synthesis uses this to map raw
    /// resource call sites — which carry `cm_name` on their
    /// `MethodInfo` — back to the right per-monomorphisation wrapper.
    pub cm_name: Option<String>,
}

/// Resource declaration captured in NIR for effect propagation.
///
/// Resources are effects in Wado's effect system: every operation on a
/// resource type requires the resource to be in scope. The `operations`
/// list mirrors `NirEffect` so the propagation closure builder can treat
/// effects and resources uniformly.
#[derive(Debug, Clone)]
pub struct NirResource {
    pub name: String,
    pub is_pub: bool,
    pub operations: Vec<NirEffectOp>,
    pub span: Span,
}

/// Trait declaration
#[derive(Debug, Clone)]
pub struct NirTrait {
    pub name: String,
    pub is_pub: bool,
    pub type_params: Vec<NirTypeParam>,
    pub methods: Vec<NirTraitMethod>,
    pub span: Span,
}

/// A method signature in a trait
#[derive(Debug, Clone)]
pub struct NirTraitMethod {
    pub name: String,
    pub params: Vec<NirParam>,
    pub return_type: TypeId,
    pub has_default_body: bool,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct NirImpl {
    /// Generic type parameters for the impl block (e.g., `impl<T> Box<T>`)
    pub type_params: Vec<NirTypeParam>,
    /// The trait being implemented, if any (e.g., "Display" for `impl Display for Type`)
    pub trait_name: Option<String>,
    pub target_type: TypeId,
    pub methods: Vec<NirFunction>,
    pub span: Span,
}

/// `impl Trait for Type;` — request the compiler to synthesize the trait implementation.
#[derive(Debug, Clone)]
pub struct SynthesisRequest {
    pub trait_name: String,
    pub target_type_name: String,
    pub target_type_id: TypeId,
    /// Type parameters: `(name, index, type_id)`
    pub type_params: Vec<(String, u32, TypeId)>,
    pub span: Span,
}

/// Metadata about a closure for optimization (especially inlining).
///
/// This is populated by the lower phase and used by the optimizer to inline
/// closure calls when the closure is known at compile time.
#[derive(Debug, Clone)]
pub struct ClosureFunctor {
    pub module_source: ModuleSource,
    /// Unique closure ID (matches the order closures are visited in the module)
    pub id: u32,
    /// Name of the generated functor struct (e.g., `__Closure_0`)
    pub struct_name: String,
    /// Type ID of the generated functor struct (bare struct type for definitions)
    pub struct_type_id: TypeId,
    /// Type ID of reference to functor struct (for expression/local types)
    /// Functors are reference types, so variables holding them have this type.
    pub ref_type_id: TypeId,
    /// The `__call` method for this closure (with body transformed:
    /// Capture nodes become `FieldAccess` on self)
    pub call_method: Rc<RefCell<NirFunction>>,
    /// Captures from the original closure
    pub captures: Vec<NirCapture>,
    /// Canonical user-declared (name, type) pairs of the closure literal —
    /// `[]` for `|| ...`, `[("x", i32)]` for `|x: i32| ...`. Captured at
    /// functor creation and never mutated. `wir_build`'s
    /// `register_closure_wrappers` uses this list to choose the wrapper's
    /// external signature: the function-table type the closure was coerced
    /// to is `fn(env, canonical_user_params...) -> canonical_return`,
    /// independent of any DAE shrinkage that happens later on
    /// `call_method.params`. Without this snapshot, dropping a "dead" param
    /// from `__call` would also shrink the wrapper signature and
    /// desynchronise it from the typed-fn callers.
    pub canonical_user_params: Vec<(String, TypeId)>,
    /// Canonical return type of the closure literal. Same role as
    /// `canonical_user_params` — drives the wrapper external signature.
    pub canonical_return: TypeId,
}

/// External function import from Component Model canonical builtins.
/// These are functions that need to be imported at the Wasm level.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NirImport {
    /// Import namespace ("wasi" or "env")
    pub namespace: String,
    /// Canonical name for the import (e.g., "stream-new", "`libm_sin`")
    pub canonical_name: String,
    /// Internal function name (e.g., "`stream_new`", "`f64_sin`")
    pub func_name: String,
    /// Parameter types
    pub params: Vec<TypeId>,
    /// Return type
    pub return_type: TypeId,
}

/// Tracks a requested instantiation of a generic item.
/// `name`, `module_source`, `impl_type_args`, and `method_type_args` are used for equality/hashing.
/// `method_info` is auxiliary metadata for name formatting.
#[derive(Debug, Clone)]
pub struct InstantiationKey {
    /// Name of the generic item (struct, function, or enum)
    pub name: String,
    /// Module where the generic item is defined.
    /// Distinguishes same-named generics from different modules.
    pub module_source: ModuleSource,
    /// Impl-level type arguments (from the struct/type)
    pub impl_type_args: Vec<TypeId>,
    /// Method-level type arguments (from the method's own generics)
    pub method_type_args: Vec<TypeId>,
    /// Method info for method instantiations (None for struct/enum instantiations)
    /// Not included in equality/hash - used only for name formatting
    pub method_info: Option<LocalMethodName>,
}

impl PartialEq for InstantiationKey {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
            && self.module_source == other.module_source
            && self.impl_type_args == other.impl_type_args
            && self.method_type_args == other.method_type_args
    }
}

impl Eq for InstantiationKey {}

impl std::hash::Hash for InstantiationKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.name.hash(state);
        self.module_source.hash(state);
        self.impl_type_args.hash(state);
        self.method_type_args.hash(state);
    }
}

#[derive(Debug, Clone)]
pub struct NirModule {
    pub module_source: ModuleSource,
    /// Shared type table across all modules (enables cross-module type references)
    pub type_table: Rc<RefCell<TypeTable>>,
    /// External function imports (canonical builtins from wasi/env namespaces)
    pub imports: Vec<NirImport>,
    pub functions: Vec<Rc<RefCell<NirFunction>>>,
    pub structs: Vec<NirStruct>,
    pub enums: Vec<NirEnum>,
    /// Flags type declarations (bitmask types, newtypes over u32)
    pub flags: Vec<NirFlags>,
    /// Custom variant declarations (tagged unions with payloads)
    pub variants: Vec<NirVariantDecl>,
    pub newtypes: Vec<NirNewtype>,
    pub effects: Vec<NirEffect>,
    pub resources: Vec<NirResource>,
    pub traits: Vec<NirTrait>,
    pub impls: Vec<NirImpl>,
    /// `impl Trait for Type;` — synthesis requests (populated by elaborator, consumed by synthesis)
    pub synthesis_requests: Vec<SynthesisRequest>,
    /// Test declarations with their metadata
    pub tests: Vec<NirTest>,
    /// Global variable declarations
    pub globals: Vec<NirGlobal>,
    pub data_section: Option<String>,
    /// `#![wasm_module("name")]` — items in this module compile to a separate Wasm core module.
    pub wasm_module: Option<String>,
    pub string_literals: Vec<String>,
    /// Byte array literals from `#include_bytes` (for data segments)
    pub bytes_literals: Vec<Vec<u8>>,
    /// Map of (`module_source`, function name) to string literals it contains (for DCE)
    pub function_strings: IndexMap<(ModuleSource, String), Vec<String>>,
    /// Map of (`module_source`, function name) to its method info (for DCE), populated alongside `function_strings`
    pub function_method_info: IndexMap<(ModuleSource, String), Option<LocalMethodName>>,
    /// Generic struct definitions (before monomorphization)
    /// Key: (struct name, module source)
    pub generic_structs: IndexMap<(String, ModuleSource), NirStruct>,
    /// Generic function definitions (before monomorphization)
    /// Key: (module source, function name).
    pub generic_functions: IndexMap<(ModuleSource, String), Rc<RefCell<NirFunction>>>,
    /// Requested instantiations (populated during resolution, processed in lower)
    pub instantiation_requests: IndexSet<InstantiationKey>,
    /// Closure metadata for optimization (populated by lower phase).
    /// Maps closure ID to functor info including the `__call` method for inlining.
    pub closure_functors: Vec<ClosureFunctor>,
}

impl NirModule {
    pub fn new(module_source: ModuleSource) -> Self {
        Self {
            module_source,
            type_table: Rc::new(RefCell::new(TypeTable::new())),
            imports: Vec::new(),
            functions: Vec::new(),
            structs: Vec::new(),
            enums: Vec::new(),
            flags: Vec::new(),
            variants: Vec::new(),
            newtypes: Vec::new(),
            effects: Vec::new(),
            resources: Vec::new(),
            traits: Vec::new(),
            impls: Vec::new(),
            synthesis_requests: Vec::new(),
            tests: Vec::new(),
            globals: Vec::new(),
            data_section: None,
            wasm_module: None,
            string_literals: Vec::new(),
            bytes_literals: Vec::new(),
            function_strings: IndexMap::default(),
            function_method_info: IndexMap::default(),
            generic_structs: IndexMap::default(),
            generic_functions: IndexMap::default(),
            instantiation_requests: IndexSet::default(),
            closure_functors: Vec::new(),
        }
    }

    pub fn with_type_table(
        module_source: ModuleSource,
        type_table: Rc<RefCell<TypeTable>>,
    ) -> Self {
        Self {
            module_source,
            type_table,
            imports: Vec::new(),
            functions: Vec::new(),
            structs: Vec::new(),
            enums: Vec::new(),
            flags: Vec::new(),
            variants: Vec::new(),
            newtypes: Vec::new(),
            effects: Vec::new(),
            resources: Vec::new(),
            traits: Vec::new(),
            impls: Vec::new(),
            synthesis_requests: Vec::new(),
            tests: Vec::new(),
            globals: Vec::new(),
            data_section: None,
            wasm_module: None,
            string_literals: Vec::new(),
            bytes_literals: Vec::new(),
            function_strings: IndexMap::default(),
            function_method_info: IndexMap::default(),
            generic_structs: IndexMap::default(),
            generic_functions: IndexMap::default(),
            instantiation_requests: IndexSet::default(),
            closure_functors: Vec::new(),
        }
    }

    pub fn with_data_section(mut self, data_section: Option<String>) -> Self {
        self.data_section = data_section;
        self
    }

    pub fn data_section(&self) -> Option<&str> {
        self.data_section.as_deref()
    }

    pub fn add_function(&mut self, func: NirFunction) -> Rc<RefCell<NirFunction>> {
        let func_rc = Rc::new(RefCell::new(func));
        self.functions.push(Rc::clone(&func_rc));
        func_rc
    }

    pub fn add_struct(&mut self, s: NirStruct) {
        self.structs.push(s);
    }

    pub fn add_enum(&mut self, e: NirEnum) {
        self.enums.push(e);
    }

    pub fn add_flags(&mut self, f: NirFlags) {
        self.flags.push(f);
    }

    pub fn add_newtype(&mut self, newtype: NirNewtype) {
        self.newtypes.push(newtype);
    }

    pub fn add_effect(&mut self, effect: NirEffect) {
        self.effects.push(effect);
    }

    pub fn add_resource(&mut self, resource: NirResource) {
        self.resources.push(resource);
    }

    pub fn add_trait(&mut self, trait_decl: NirTrait) {
        self.traits.push(trait_decl);
    }

    pub fn add_impl(&mut self, impl_block: NirImpl) {
        self.impls.push(impl_block);
    }

    pub fn find_function(&self, name: &str) -> Option<Rc<RefCell<NirFunction>>> {
        self.functions
            .iter()
            .find(|f| f.borrow().name == name)
            .cloned()
    }

    pub fn find_struct(&self, name: &str) -> Option<&NirStruct> {
        self.structs.iter().find(|s| s.name == name)
    }

    pub fn find_enum(&self, name: &str) -> Option<&NirEnum> {
        self.enums.iter().find(|e| e.name == name)
    }
}

#[derive(Debug)]
pub struct NirProgram {
    pub main_module: NirModule,
    pub dependencies: Vec<NirModule>,
    pub type_table: TypeTable,
}

impl NirProgram {
    pub fn new(main_module: NirModule) -> Self {
        Self {
            type_table: TypeTable::new(),
            main_module,
            dependencies: Vec::new(),
        }
    }
}

// (Type-system unit tests live in `crate::tir`'s test module; NIR shares
// the TIR `TypeTable` and has nothing additional to assert here yet.)
