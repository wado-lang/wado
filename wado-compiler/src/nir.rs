//! Normalized Intermediate Representation (NIR): the post-lower body IR
//! `optimize` and `wir_build` consume. Bodies live in the skeleton arena
//! (`crate::nir_arena`); this module holds the metadata around them — functions,
//! globals, params, locals, captures, refs, and the shared leaf enums. Type
//! identity and `EffectRef` are shared with TIR. See WEP 2026-05-11.

use std::cell::RefCell;
use std::cmp::Ordering;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering as MemoryOrdering};
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};

use crate::ast;
use crate::compiler_item::CompilerItem;
use crate::hashmap::{IndexMap, IndexSet};

use crate::module_source::ModuleSource;
use crate::name::{
    FunctionId, LocalMethodName, closure_call_method_info, closure_call_name, minted_name,
};
use crate::nir_arena::{Body, ExprBody, Tracked};
use crate::tir::{self, DeclarationLookup, EffectRef, StructDef, TypeId, TypeTable};
use crate::token::Span;

/// Canonical identity of a function entity. Minted in `lower` over the
/// post-monomorphization function set and intrinsic to the entity — not its
/// storage position, so it is stable across `dce` compaction. The mangled
/// `name` is a lookup attribute, never identity.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FuncId(u32);
cranelift_entity::entity_impl!(FuncId, "fn");

#[derive(Debug, Clone)]
pub struct FunctionRef {
    pub module_source: ModuleSource,
    pub name: String,
    pub monomorph_info: Option<MonomorphInfo>,
    pub method_info: Option<LocalMethodName>,
}

impl<'a> From<&'a FunctionRef> for DeclarationLookup<'a> {
    fn from(func: &'a FunctionRef) -> Self {
        Self {
            module_source: &func.module_source,
            name: &func.name,
            generic_name: func
                .monomorph_info
                .as_ref()
                .map(|m| m.generic_name.as_str()),
        }
    }
}

impl<'a> From<&'a NirFunction> for DeclarationLookup<'a> {
    fn from(func: &'a NirFunction) -> Self {
        Self {
            module_source: &func.module_source,
            name: &func.name,
            generic_name: func
                .monomorph_info
                .as_ref()
                .map(|m| m.generic_name.as_str()),
        }
    }
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
        } else {
            let path = self.module_source.to_path();
            format!("{}/{}", path.join("/"), self.name)
        }
    }

    pub fn intrinsic(&self) -> Option<&str> {
        DeclarationLookup::from(self).intrinsic()
    }

    /// Whether this is the `core:builtin` intrinsic `builtin`.
    pub fn is_builtin_named(&self, builtin: &str) -> bool {
        self.intrinsic() == Some(builtin)
    }

    /// The `$call` of closure functor `functor_id`, under the mangled name
    /// `lower` mints. Spelled a second way it would be a second key.
    pub fn closure_call(module: &ModuleSource, functor_id: u32) -> Self {
        Self {
            module_source: module.clone(),
            name: closure_call_name(module, functor_id),
            monomorph_info: None,
            method_info: Some(closure_call_method_info(module, functor_id)),
        }
    }

    /// Check if this function is monomorphized (instantiated from a generic)
    pub fn is_monomorphized(&self) -> bool {
        self.monomorph_info.is_some()
    }

    /// Check if this is a method (instance or static) as opposed to a free function.
    pub fn is_method(&self) -> bool {
        self.method_info.is_some()
    }

    /// The canonical [`crate::name::FunctionId`] this reference denotes, keyed on
    /// `(module_source, mangled name)`. The mangled `name` already encodes a
    /// method's `struct^trait::method` and any monomorphization type args, so it
    /// is the one injective identity for a function — independent of whether a
    /// *call site* happens to carry `monomorph_info` (which flipped the older
    /// `Method`/`Free` split and made the same callee key two different ways).
    /// Used to mint and stamp `FuncId`s in `lower`.
    pub fn function_id(&self) -> FunctionId {
        use crate::name::FunctionId;
        FunctionId::free(&self.module_source, &self.name)
    }
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
    RefNotEq,
}

impl NirBinaryOp {
    /// The bool operand this op passes the other one through for: `true & x`,
    /// `false | x` and `false ^ x` are `x`, as are their `&&` / `||` forms.
    pub fn bool_identity(self) -> Option<bool> {
        match self {
            Self::And | Self::BitAnd => Some(true),
            Self::Or | Self::BitOr | Self::BitXor => Some(false),
            _ => None,
        }
    }

    /// What this comparison answers for two values `ordering` relates. Every
    /// type's comparisons read an order, a float's included. `None` for an op
    /// that is not one of the six.
    pub fn holds_for(self, ordering: Ordering) -> Option<bool> {
        match self {
            Self::Eq => Some(ordering.is_eq()),
            Self::NotEq => Some(ordering.is_ne()),
            Self::Lt => Some(ordering.is_lt()),
            Self::LtEq => Some(ordering.is_le()),
            Self::Gt => Some(ordering.is_gt()),
            Self::GtEq => Some(ordering.is_ge()),
            Self::Add
            | Self::Sub
            | Self::Mul
            | Self::Div
            | Self::Mod
            | Self::And
            | Self::Or
            | Self::BitAnd
            | Self::BitOr
            | Self::BitXor
            | Self::Shl
            | Self::Shr
            | Self::RefNotEq => None,
        }
    }

    /// Whether this op may trap, whatever its operands. Integer `Div` / `Mod`
    /// trap on a zero divisor (and `MIN / -1`); every other binary op is total.
    /// The one listing every pass that deletes or moves an operation consults.
    pub fn may_trap(self) -> bool {
        matches!(self, Self::Div | Self::Mod)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NirUnaryOp {
    Neg,
    Not,
    BitNot,
    Ref,
    MutRef,
    Deref,
}

impl NirUnaryOp {
    /// Whether the value pool has a `ValueKind` for this op. A borrow or a
    /// dereference names a place, and no kind carries one.
    pub fn is_pooled(self) -> bool {
        !matches!(self, Self::Ref | Self::MutRef | Self::Deref)
    }

    /// Whether this op may trap, whatever its operand. `Deref` traps on a null
    /// reference; `Ref` / `MutRef` / `Neg` / `Not` / `BitNot` are total.
    pub fn may_trap(self) -> bool {
        matches!(self, Self::Deref)
    }
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

/// Generic type parameter in NIR (from AST `GenericParam`)
#[derive(Debug, Clone, PartialEq)]
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
#[derive(Debug, Clone, PartialEq)]
pub struct MonomorphInfo {
    /// Original generic name: `"Box"` for `"Box<i32>"`, or
    /// `"BTreeNode<K,V>::insert"` for methods.
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
    /// How the storage gets its value. Either case holds an
    /// [`crate::nir_arena::ExprBody`] (a single-`Expr`-statement arena `Body`;
    /// read it via `.expr()`), arena-shaped like function bodies so the
    /// optimizer passes share one representation.
    pub init: tir::GlobalInit<ExprBody>,
    /// Whether the program may assign to this global — `global mut`. The Wasm
    /// slot's mutability is wider and derived when the module is built.
    pub wado_mutable: bool,
    pub visibility: ast::Visibility,
    /// Module where this global is defined
    pub module_source: ModuleSource,
    pub span: Span,
    /// Per-local metadata for the initializer expression. Populated when
    /// the initializer is non-trivial (e.g. a literal coercion). Indexed by
    /// local index, like `NirFunction::locals`.
    pub locals: Vec<NirLocal>,
    /// True for a global `const_object_globalization` created from an
    /// `InlineRef` candidate (a hoisted `&`-literal call argument, rebuilt
    /// at its call site on every use until promoted eager). Such a
    /// `GlobalVarSet` needs a higher, bounded byte threshold than
    /// `NirPackage::string_inline_max_bytes` to promote to `array.new_fixed`
    /// — otherwise `wir_optimize::const_global` can't prove the repeated
    /// assignment redundant, and hoisting buys nothing. `false` for every
    /// other global, including a `LetBinding` candidate.
    pub prefer_fixed_string_repr: bool,
    /// The `#[param]` name, when the global carries one. Kept past TIR so a
    /// remark can say *which* compile-time parameter failed to fold — a remark
    /// that cannot name it stays silent.
    pub param_name: Option<String>,
}

/// A NIR function as the package holds it: shared by `Arc` so the optimizer's
/// threads reach it, and counting the mutable borrows taken of it so a memo
/// keyed on the count cannot outlive a rewrite (WEP: Parallel Optimizer).
pub struct FuncCell {
    func: RwLock<NirFunction>,
    writes: AtomicU64,
}

/// How the package and its passes hold a [`FuncCell`].
pub type FuncRef = Arc<FuncCell>;

impl FuncCell {
    #[must_use]
    pub fn new(func: NirFunction) -> FuncRef {
        Arc::new(Self {
            func: RwLock::new(func),
            writes: AtomicU64::new(0),
        })
    }

    /// Read the function. Panics while another borrow writes it, as
    /// `RefCell::borrow` does: no caller waits for a writer.
    pub fn borrow(&self) -> RwLockReadGuard<'_, NirFunction> {
        self.func
            .try_read()
            .expect("a NIR function read while it is written")
    }

    /// Read the function, or `None` while a borrow writes it.
    pub fn try_borrow(&self) -> Option<RwLockReadGuard<'_, NirFunction>> {
        self.func.try_read().ok()
    }

    /// Borrow the function for writing. Panics while another borrow holds it,
    /// as `RefCell::borrow_mut` does. The guard counts one write on drop if the
    /// function changed: its body or locals by their [`Tracked`] versions, the
    /// rest against a copy taken on the first whole-function mutable borrow. A
    /// borrow that changes nothing counts nothing.
    pub fn borrow_mut(&self) -> FuncWriteGuard<'_> {
        let guard = self
            .func
            .try_write()
            .expect("a NIR function written while it is borrowed");
        FuncWriteGuard {
            version: guard.version(),
            guard,
            writes: &self.writes,
            head: None,
        }
    }

    /// How many borrows have written the function: what a memo of its facts
    /// is keyed by.
    pub fn writes(&self) -> u64 {
        self.writes.load(MemoryOrdering::Relaxed)
    }
}

/// [`FuncCell::borrow_mut`]'s guard.
pub struct FuncWriteGuard<'a> {
    guard: RwLockWriteGuard<'a, NirFunction>,
    writes: &'a AtomicU64,
    /// [`NirFunction::version`] when the guard was taken.
    version: (Option<[u64; 10]>, u64),
    /// The function's [`Head`] before the first whole-function mutable borrow.
    head: Option<Head>,
}

/// Everything of a function but its body and locals, which count their own
/// edits: what a whole-function mutable borrow may change unseen.
#[derive(PartialEq)]
struct Head {
    id: Option<FuncId>,
    is_dead: bool,
    name: String,
    module_source: ModuleSource,
    visibility: ast::Visibility,
    is_export: bool,
    is_async: bool,
    type_params: Vec<NirTypeParam>,
    impl_type_params: Vec<NirTypeParam>,
    monomorph_info: Option<MonomorphInfo>,
    method_info: Option<LocalMethodName>,
    params: Vec<NirParam>,
    return_type: TypeId,
    task_return_type: Option<TypeId>,
    effects: Vec<EffectRef>,
    retains: Vec<String>,
    span: Span,
    address_taken_locals: IndexSet<u32>,
    stores_aliased_locals: IndexSet<u32>,
    is_cm_binding: bool,
    is_dispatch_wrapper: bool,
    is_cm_export: bool,
    is_ambient: bool,
    inline_hint: InlineHint,
    compiler_item: Option<CompilerItem>,
    export_name: Option<String>,
    allocator_tag: Option<String>,
    kind: FunctionKind,
    scalarized_from: Option<TypeId>,
    return_abi: ReturnAbi,
}

impl Head {
    fn of(func: &NirFunction) -> Self {
        let NirFunction {
            id,
            is_dead,
            name,
            module_source,
            visibility,
            is_export,
            is_async,
            type_params,
            impl_type_params,
            monomorph_info,
            method_info,
            params,
            return_type,
            task_return_type,
            effects,
            retains,
            span,
            address_taken_locals,
            stores_aliased_locals,
            is_cm_binding,
            is_dispatch_wrapper,
            is_cm_export,
            is_ambient,
            inline_hint,
            compiler_item,
            export_name,
            allocator_tag,
            kind,
            scalarized_from,
            return_abi,
            body: _,
            locals: _,
        } = func;
        Self {
            id: *id,
            is_dead: *is_dead,
            name: name.clone(),
            module_source: module_source.clone(),
            visibility: *visibility,
            is_export: *is_export,
            is_async: *is_async,
            type_params: type_params.clone(),
            impl_type_params: impl_type_params.clone(),
            monomorph_info: monomorph_info.clone(),
            method_info: method_info.clone(),
            params: params.clone(),
            return_type: *return_type,
            task_return_type: *task_return_type,
            effects: effects.clone(),
            retains: retains.clone(),
            span: *span,
            address_taken_locals: address_taken_locals.clone(),
            stores_aliased_locals: stores_aliased_locals.clone(),
            is_cm_binding: *is_cm_binding,
            is_dispatch_wrapper: *is_dispatch_wrapper,
            is_cm_export: *is_cm_export,
            is_ambient: *is_ambient,
            inline_hint: *inline_hint,
            compiler_item: *compiler_item,
            export_name: export_name.clone(),
            allocator_tag: allocator_tag.clone(),
            kind: kind.clone(),
            scalarized_from: *scalarized_from,
            return_abi: return_abi.clone(),
        }
    }
}

/// The parts of a function a rewrite of its body touches: the body and the
/// locals mutably, which count their own edits, and the rest to read.
pub struct FuncParts<'a> {
    pub body: Option<&'a mut Body>,
    pub locals: &'a mut Tracked<Vec<NirLocal>>,
    pub name: &'a str,
    pub params: &'a [NirParam],
    pub address_taken_locals: &'a IndexSet<u32>,
    pub stores_aliased_locals: &'a IndexSet<u32>,
}

impl FuncWriteGuard<'_> {
    /// The body and locals to rewrite, without counting a write for taking
    /// them: the write is counted on drop if either moved.
    pub fn parts(&mut self) -> FuncParts<'_> {
        let NirFunction {
            body,
            locals,
            name,
            params,
            address_taken_locals,
            stores_aliased_locals,
            ..
        } = &mut *self.guard;
        FuncParts {
            body: body.as_mut(),
            locals,
            name,
            params,
            address_taken_locals,
            stores_aliased_locals,
        }
    }
}

impl Drop for FuncWriteGuard<'_> {
    fn drop(&mut self) {
        let head_moved = self
            .head
            .as_ref()
            .is_some_and(|head| *head != Head::of(&self.guard));
        if head_moved || self.guard.version() != self.version {
            self.writes.fetch_add(1, MemoryOrdering::Relaxed);
        }
    }
}

impl std::ops::Deref for FuncWriteGuard<'_> {
    type Target = NirFunction;

    fn deref(&self) -> &NirFunction {
        &self.guard
    }
}

impl std::ops::DerefMut for FuncWriteGuard<'_> {
    fn deref_mut(&mut self) -> &mut NirFunction {
        if self.head.is_none() {
            self.head = Some(Head::of(&self.guard));
        }
        &mut self.guard
    }
}

impl std::fmt::Debug for FuncCell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.borrow().fmt(f)
    }
}

#[derive(Debug, Clone)]
pub struct NirFunction {
    /// This function's canonical [`FuncId`], set by
    /// `NirPackage::assign_func_ids` at the end of `lower`. `None` until then
    /// (and for optimizer-synthesized functions not yet minted). Analyses key
    /// per-function facts by this id via `SecondaryMap<FuncId, _>`.
    pub id: Option<FuncId>,
    /// Liveness bit. `dce` sets this `true` for an unreachable function instead
    /// of removing it from the store, so `FuncId == position` holds for the whole
    /// pipeline. A dead
    /// function lingers as an inert bodyless record; `wir_build` skips it. Distinct
    /// from a live-but-bodyless declaration (extern / `FnCanonicalDispatch`), which
    /// keeps `is_dead == false`.
    pub is_dead: bool,
    pub name: String,
    /// Module this function belongs to. Set by the link phase when flattening
    /// per-module body data into flat lists; before link, the `module_source` is
    /// carried implicitly by the parent `NirModule`.
    pub module_source: ModuleSource,
    pub visibility: ast::Visibility,
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
    /// The result an `async fn` delivers through `task return`. `None` for a
    /// non-async fn and for a synthesized wrapper. The effect checker reads it
    /// to infer signature resources.
    pub task_return_type: Option<TypeId>,
    pub effects: Vec<EffectRef>,
    /// Parameter names the function keeps past its return.
    pub retains: Vec<String>,
    pub body: Option<Body>,
    pub span: Span,
    /// Per-local metadata — `name`, `type_id`, `is_mut` — indexed by Wasm
    /// local index. Entries `0..params.len()` shadow the corresponding
    /// `params[i]` (for uniform absolute indexing); body let-bindings and
    /// elaborator/optimizer-allocated temporaries occupy `params.len()..`.
    /// `locals.len()` *is* the local count: passes that grow the local set push
    /// a `NirLocal` per new index, so the next free index is always
    /// [`NirFunction::local_count`].
    pub locals: Tracked<Vec<NirLocal>>,
    /// Local indices that have their address taken (&x or &mut x).
    /// For mutable primitives, these locals are stored in box structs.
    pub address_taken_locals: IndexSet<u32>,

    /// Local indices a decomposed struct's field held a reference to, which
    /// SROA must not decompose in turn. Written by SROA alone; every earlier
    /// phase leaves it empty and later ones only carry and remap it.
    pub stores_aliased_locals: IndexSet<u32>,

    /// Whether this function is a synthesized CM binding (generated by `synthesis::cm_binding`).
    /// The inliner and effect checker both skip CM bindings because they are ABI bridges
    /// between Wado GC types and CM linear memory with special effect semantics.
    pub is_cm_binding: bool,

    /// Whether this function is a synthesised effect-dispatch wrapper
    /// (generated by `synthesis::effect_dispatch`). Effect-operation
    /// call-site rewriting must skip these — their fallback path
    /// directly calls `$cm_binding__<E>_<op>`, which would loop back
    /// through the wrapper if rewritten.
    pub is_dispatch_wrapper: bool,

    /// Whether this function is a synthesized CM *export* binding (world export wrapper).
    /// When true, the global initializer (`$initialize_modules`) is injected at the start
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
    pub compiler_item: Option<CompilerItem>,

    /// Custom wasm export name from `#[export_name("...")]` attribute.
    pub export_name: Option<String>,

    /// Allocator tag from `#[allocator("...")]` attribute (e.g., `"bump"`, `"debug"`).
    pub allocator_tag: Option<String>,

    /// Categorizes the function for kind-specific optimizations. Most functions
    /// are `Regular`; synthesis passes set specialized kinds so the NIR
    /// optimizer can apply targeted transformations (e.g. freshness-based
    /// elision for `ValueCopy`).
    pub kind: FunctionKind,

    /// The variant this function's return used to be, when
    /// `optimize::sroa_variant_return` rewrote it into a `[tag, slots…]` tuple.
    /// `None` for every function it did not touch. Read by that pass's repair
    /// step, which has to recognise its own earlier work in a later iteration.
    pub scalarized_from: Option<TypeId>,

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

/// How a parameter arrives at the Wasm level. The mirror of [`ReturnAbi`]: Wasm
/// takes N parameters as freely as it returns N results.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ParamAbi {
    /// One Wasm parameter, of the NIR type as written.
    #[default]
    Single,
    /// One Wasm parameter per field, in declaration order. The NIR type and
    /// local index are unchanged; only the WIR-level ABI shifts.
    MultiValue {
        field_types: Vec<TypeId>,
        field_names: Vec<String>,
    },
}

/// Semantic category of a `NirFunction`. Carries the type operand so the
/// optimizer can reason about the call without re-deriving it from the
/// signature.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum FunctionKind {
    /// Ordinary user-defined or synthesized function.
    #[default]
    Regular,
    /// Synthesized `copy_value` function that deep-copies a value of
    /// `type_id`. Calls to such functions may be elided when the argument is
    /// provably fresh.
    ValueCopy { type_id: TypeId },
    /// Auto-derived `fn(..)^Inspect::inspect` dispatch stub. Its NIR body is
    /// `unreachable()`, a placeholder making the call resolvable;
    /// WIR build recognises the kind and supplies a `call_ref` through the
    /// matching `CanonicalClosure_K` vtable slot. `(arity, return_type)` are
    /// structured fields so nothing has to parse the mangled name.
    FnCanonicalDispatch { arity: usize, return_type: TypeId },
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
    /// Different after any edit to the body or the locals.
    pub fn version(&self) -> (Option<[u64; 10]>, u64) {
        (self.body.as_ref().map(Body::version), self.locals.version())
    }

    /// [`Body::calls_any`] of the body, `false` for a bodyless function.
    pub fn calls_any(&self, callee: impl Fn(&FuncId) -> bool) -> bool {
        self.body.as_ref().is_some_and(|b| b.calls_any(callee))
    }

    /// Bodyless stub for an extern / builtin callee (Phase 5 interning).
    pub fn extern_stub(func_ref: &FunctionRef) -> Self {
        Self {
            id: None,
            is_dead: false,
            name: func_ref.name.clone(),
            module_source: func_ref.module_source.clone(),
            visibility: ast::Visibility::Private,
            is_export: false,
            is_async: false,
            type_params: Vec::new(),
            impl_type_params: Vec::new(),
            monomorph_info: func_ref.monomorph_info.clone(),
            method_info: func_ref.method_info.clone(),
            params: Vec::new(),
            return_type: TypeTable::UNIT,
            task_return_type: None,
            effects: Vec::new(),
            retains: Vec::new(),
            body: None,
            span: Span::default(),
            locals: Tracked::new(Vec::new()),
            address_taken_locals: IndexSet::default(),
            stores_aliased_locals: IndexSet::default(),
            is_cm_binding: false,
            is_dispatch_wrapper: false,
            is_cm_export: false,
            is_ambient: false,
            inline_hint: InlineHint::default(),
            compiler_item: None,
            export_name: None,
            allocator_tag: None,
            kind: FunctionKind::default(),
            scalarized_from: None,
            return_abi: ReturnAbi::default(),
        }
    }

    /// The number of locals (params + body locals), i.e. the next free local
    /// index. `locals` is the single source of truth — there is no separate
    /// count field — so a pass allocates a local by pushing a `NirLocal` and
    /// taking the new `locals.len() - 1` as its index.
    #[inline]
    pub fn local_count(&self) -> u32 {
        self.locals.len() as u32
    }

    /// Returns true if this is a method (belongs to a struct)
    #[inline]
    pub fn is_method(&self) -> bool {
        self.method_info.is_some()
    }

    /// Returns true if this is the synthesized `$call` method on a
    /// `$Closure_N` functor struct. See
    /// [`LocalMethodName::is_closure_call`] for the rationale.
    #[inline]
    pub fn is_closure_call(&self) -> bool {
        self.method_info
            .as_ref()
            .is_some_and(LocalMethodName::is_closure_call)
    }

    /// Returns true if this function has type params that need monomorphization
    /// (excludes effect params, which are erased at compile time).
    #[inline]
    pub fn has_real_type_params(&self) -> bool {
        self.type_params.iter().any(|p| !p.is_effect)
    }

    /// Whether every caller reaches this function through a direct call that
    /// `wir_build` lowers against its recorded ABI, so changing that ABI is safe.
    // A monomorphized trait method qualifies. `ValueCopy` and
    // `FnCanonicalDispatch` do not: neither is reached through a NIR call node,
    // so nothing at the call site would follow the signature.
    #[inline]
    pub fn only_reached_by_direct_call(&self) -> bool {
        matches!(self.kind, FunctionKind::Regular)
            && !self.is_dispatch_wrapper
            && !self.is_export
            && !self.is_cm_export
            && !self.is_cm_binding
            && !self.is_async
            && !self.has_real_type_params()
            && self.impl_type_params.is_empty()
            && !self.is_closure_call()
    }

    /// Returns the copied type if this is a synthesized value-copy function.
    #[inline]
    pub fn value_copy_type(&self) -> Option<TypeId> {
        match self.kind {
            FunctionKind::ValueCopy { type_id } => Some(type_id),
            _ => None,
        }
    }

    /// Dispatch coordinates of an auto-derived `fn(..)^Inspect` stub, which WIR
    /// build turns into the indirect-call body.
    #[inline]
    pub fn fn_canonical_dispatch(&self) -> Option<(usize, TypeId)> {
        match self.kind {
            FunctionKind::FnCanonicalDispatch { arity, return_type } => Some((arity, return_type)),
            _ => None,
        }
    }

    /// Returns true if this function was synthesized as a value-copy helper.
    #[inline]
    pub fn is_value_copy(&self) -> bool {
        matches!(self.kind, FunctionKind::ValueCopy { .. })
    }
}

/// A resolved local-slot entry in a function, global initializer, or closure
/// scope, identified by its order in the surrounding local environment.
/// `FunctionContext::locals` is the single source of truth, projected onto
/// `NirFunction::locals` / `NirGlobal::locals` (keyed by Wasm local index) and
/// onto a closure's `params + body_locals`, which do not overlap.
#[derive(Debug, Clone)]
pub struct NirLocal {
    /// Source-level name of the binding (or a synthesised `$name` for
    /// elaborator-generated temporaries that have no surface syntax).
    pub name: String,
    pub type_id: TypeId,
    pub is_mut: bool,
}

impl NirLocal {
    /// Build a `NirLocal` for a synthesised slot whose name follows the
    /// `$local_N` convention used by `wir_build` when no source-level
    /// name is available.
    pub fn synth(index: u32, type_id: TypeId, is_mut: bool) -> Self {
        Self {
            name: minted_name("local", index),
            type_id,
            is_mut,
        }
    }

    /// Push onto `locals` a local a pass mints, named by [`minted_name`] from
    /// `what` and the index it takes, returning that index.
    pub fn push_minted(locals: &mut Vec<Self>, what: &str, type_id: TypeId, is_mut: bool) -> u32 {
        let index = u32::try_from(locals.len()).expect("local index overflow");
        locals.push(Self {
            name: minted_name(what, index),
            type_id,
            is_mut,
        });
        index
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct NirParam {
    pub name: String,
    pub type_id: TypeId,
    pub local_index: u32,
    pub is_mut: bool,
    /// The parameter is a `&mut T` borrow — the only kind that can mutate the
    /// caller's argument storage. Captured pre-boxing (see [`crate::tir::TirParam::is_mut_ref`]).
    pub is_mut_ref: bool,
    pub span: Span,
    /// How this parameter arrives at the Wasm level. Set by
    /// `optimize::multi_value_param`; `Single` everywhere else.
    pub param_abi: ParamAbi,
}

#[derive(Debug, Clone)]
pub struct NirStruct {
    /// The struct type this reifies — the declaration it was written from, or
    /// the shape it was built from. Carried down from `TirStruct` so a pass
    /// asking which struct this is does not have to find one by `name`.
    pub def: StructDef,
    pub name: String,
    pub module_source: ModuleSource,
    pub visibility: ast::Visibility,
    /// Generic type parameters (empty for non-generic structs)
    pub type_params: Vec<NirTypeParam>,
    /// If this struct was created by monomorphization, contains the origin info
    pub monomorph_info: Option<MonomorphInfo>,
    pub fields: Vec<NirField>,
    pub span: Span,
    /// `#[wire(name_policy = "...")]` — naming strategy for all fields.
    pub wire_name_policy: Option<ast::NamePolicy>,
}

#[derive(Debug, Clone)]
pub struct NirField {
    pub name: String,
    pub visibility: ast::Visibility,
    pub type_id: TypeId,
    pub index: u32,
    pub span: Span,
    /// `#[secret]` — field not shown in debug inspect output.
    pub is_secret: bool,
    /// `#[wire(name = "name")]` — custom serialization name for this field.
    pub wire_name_override: Option<String>,
    /// `#[wire(default)]` — use default value when field is missing during deserialization.
    pub serde_default: bool,
}

#[derive(Debug, Clone)]
pub struct NirEnum {
    pub name: String,
    pub module_source: ModuleSource,
    pub visibility: ast::Visibility,
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
    pub visibility: ast::Visibility,
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
    pub visibility: ast::Visibility,
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
    pub visibility: ast::Visibility,
    pub type_id: TypeId,
    pub span: Span,
}

/// Test declaration metadata
/// The actual test code is stored as a `NirFunction` in the functions list.
#[derive(Debug, Clone)]
pub struct NirTest {
    /// The original test name from source (None if unnamed)
    pub name: Option<String>,
    /// Generated function name (e.g., "$`test_0`", "$`test_trap_0`", or "$`test_todo_0`")
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
    pub visibility: ast::Visibility,
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
    pub visibility: ast::Visibility,
    pub operations: Vec<NirEffectOp>,
    pub span: Span,
}

/// Trait declaration
#[derive(Debug, Clone)]
pub struct NirTrait {
    pub name: String,
    pub visibility: ast::Visibility,
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
    /// Name of the generated functor struct (e.g., `$Closure_0`)
    pub struct_name: String,
    /// Type ID of the generated functor struct (bare struct type for definitions)
    pub struct_type_id: TypeId,
    /// Type ID of reference to functor struct (for expression/local types)
    /// Functors are reference types, so variables holding them have this type.
    pub ref_type_id: TypeId,
    /// The `$call` method for this closure (with body transformed:
    /// Capture nodes become `FieldAccess` on self)
    pub call_method: FuncRef,
    /// The per-functor `$Closure_N^Inspect::inspect` impl. Found through here
    /// and not by name: `dae` renames what it reshapes.
    pub inspect_method: FuncRef,
    /// Canonical user-declared (name, type) pairs of the closure literal,
    /// captured at functor creation and never mutated.
    /// `register_closure_wrappers` reads it for the wrapper's external signature
    /// `fn(env, canonical_user_params...) -> canonical_return`, so later DAE
    /// shrinkage on `call_method.params` cannot desynchronise typed-fn callers.
    pub canonical_user_params: Vec<(String, TypeId)>,
    /// Canonical return type of the closure literal. Same role as
    /// `canonical_user_params` — drives the wrapper external signature.
    pub canonical_return: TypeId,
}

/// External function import from Component Model canonical builtins.
/// These are functions that need to be imported at the Wasm level.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NirImport {
    /// The module declaring `func_name`.
    pub module_source: ModuleSource,
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
    /// Method info for method instantiations (None for struct/enum instantiations).
    /// Left out of equality and hash, so it names an instance but never decides
    /// one: read a declaration's own `method_info` for anything else.
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
    pub functions: Vec<FuncRef>,
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
    /// Closure metadata for optimization (populated by lower phase).
    /// Maps closure ID to functor info including the `$call` method for inlining.
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
            synthesis_requests: Vec::new(),
            tests: Vec::new(),
            globals: Vec::new(),
            data_section: None,
            wasm_module: None,
            string_literals: Vec::new(),
            bytes_literals: Vec::new(),
            function_strings: IndexMap::default(),
            function_method_info: IndexMap::default(),
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
            synthesis_requests: Vec::new(),
            tests: Vec::new(),
            globals: Vec::new(),
            data_section: None,
            wasm_module: None,
            string_literals: Vec::new(),
            bytes_literals: Vec::new(),
            function_strings: IndexMap::default(),
            function_method_info: IndexMap::default(),
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

    pub fn add_function(&mut self, func: NirFunction) -> FuncRef {
        let func_rc = FuncCell::new(func);
        self.functions.push(Arc::clone(&func_rc));
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

    pub fn find_function(&self, name: &str) -> Option<FuncRef> {
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
