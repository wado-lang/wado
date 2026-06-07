//! NIR Interpreter (niri).
//!
//! Compile-time partial evaluator for Wado NIR, operating on the arena `Body`.
//! [`Interpreter::reduce_local_a`] rewrites one node in place toward literal
//! form, [`Interpreter::reduce_to_lattice_a`] projects a node to a [`Lattice`],
//! and [`Interpreter::reduce_in_place_a`] reduces a whole subtree bottom-up.
//! Constant folding is the primary consumer; branch pruning, constant
//! propagation, and compile-time function evaluation reuse the same engine.
//!
//! Reduction is **monotone** — it only moves expressions toward literal form,
//! never the reverse — and **idempotent**. Literal leaves are preserved as-is
//! so the original lexical repr (e.g. `0xFF`) survives a no-op pass.
//!
//! The engine handles:
//!
//! - Integer arithmetic: Add, Sub, Mul, Div, Mod
//! - Integer comparison: Eq, `NotEq`, Lt, `LtEq`, Gt, `GtEq`
//! - Integer bitwise: `BitAnd`, `BitOr`, `BitXor`, Shl, Shr
//! - Integer unary: Neg, `BitNot`
//! - Integer types: i8, i16, i32, i64, u8, u16, u32, u64
//! - Float arithmetic: Add, Sub, Mul, Div (skipped when result is NaN)
//! - Float comparison: Eq, `NotEq`, Lt, `LtEq`, Gt, `GtEq`
//! - Float unary: Neg (via sign-bit flip, safe for all values including NaN)
//! - Float types: f32, f64
//! - Boolean logical: And, Or (including identity rules `false || X → X`,
//!   `true && X → X`, `X || false → X`, `X && true → X`)
//! - Boolean equality and ordering: Eq, `NotEq`, Lt, `LtEq`, Gt, `GtEq`
//!   (`false < true`)
//! - Boolean unary: Not
//! - Char comparison: Eq, `NotEq`, Lt, `LtEq`, Gt, `GtEq` (codepoint
//!   order)
//! - Casts (`expr as T`):
//!   - int ↔ int (truncation / sign- or zero-extension)
//!   - int ↔ float (signed / unsigned conversion; float → int uses
//!     Wasm `trunc_sat` semantics: NaN ↦ 0, ±∞ saturate to MIN/MAX)
//!   - f32 ↔ f64 (rounding on demote, exact on promote)
//!   - bool → int / float (true ↦ 1 / 1.0, false ↦ 0 / 0.0)
//!   - char → int (codepoint, then truncated to target width)
//!   - u8 → char (the only int → char form the elaborator permits)
//! - Local variables: immutable `let` bindings whose RHS reduces to a
//!   constant flow into the env and are read back as that constant at
//!   each use site. `let mut` and post-assign locals stay `NonConst`.
//!   The driving visitor populates the env via
//!   [`Interpreter::bind_local`] / [`Interpreter::invalidate_local`].
//! - Global variables: immutable `global FOO: T = …;` declarations
//!   whose initializer reduces to a constant flow into a project-wide
//!   [`GlobalEnv`] and are read back at every `GlobalVarGet` site.
//!   Mutable globals are recorded as `NonConst` so a parent fold like
//!   `GLOBAL_MUT + 1` reports `NonConst` rather than `Unevaluated`. The
//!   driving visitor builds the env once per pass via
//!   [`Interpreter::with_globals`].
//! - `if` expressions: a constant condition collapses to the chosen
//!   arm; a non-constant condition with both arms reducing to the same
//!   lattice constant (and an effect-free condition) folds to that
//!   constant. The unreachable arm of a constant-condition `if` is
//!   treated as an SCCP infeasible edge, so a trapping branch
//!   (`else { panic(…) }`) does not contaminate the result.
//! - `if` statements: a constant condition splices the chosen branch's
//!   stmts into the parent block via
//!   [`Interpreter::reduce_local_block`].
//! - `match` expressions: a constant scrutinee collapses to the first
//!   arm whose pattern provably matches (later arms become infeasible
//!   edges); a non-constant speculatable scrutinee with every arm
//!   reducing to the same lattice constant collapses to that constant.
//!   Modelled patterns: `_`, integer / bool / char literal, integer
//!   range (signed and unsigned), or-of the above, and `ConstantValue`
//!   whose inner expression reduces to a primitive `Value`. `Binding`,
//!   `Tuple`, `Variant`, `Struct`, `Enum`, and string / null literal
//!   patterns report `Unknown` — they never wrongly commit a match and
//!   never wrongly drop a later arm.
//! - Pure-call inlining: a free `Call` whose args all reduce to
//!   constants and whose callee was admitted to the [`CalleeMap`]
//!   (pure, non-async, monomorphic — see [`is_ctfe_eligible`]) and
//!   whose body is a single `Return { Some(_) }` or `Expr(_)`
//!   evaluates the body's tail with the args bound into a fresh local
//!   environment. The `call_stack` of in-flight callees blocks
//!   recursive re-entry; a per-pass step budget caps total CTFE work;
//!   the dynamic borrow on the shared callee `RefCell` blocks the
//!   visitor's outer `borrow_mut`. `NonConst` tail results (e.g. body
//!   contains a runtime div-by-zero) are downgraded to Unevaluated so
//!   the original Call survives and the runtime trap is preserved.
//!   `MethodCall` / `IndirectCall` / `CmRawCall` and multi-stmt bodies
//!   are out of scope.
//!
//! Float arithmetic uses native Rust IEEE 754 ops (same as Wasm), following
//! cranelift's approach: fold the result, but skip if it is NaN since NaN
//! bit patterns are nondeterministic across architectures.
//!
//! Integer division/modulo by zero and signed `MIN / -1` are left
//! unfolded so the runtime trap is preserved.
//!
//! See `docs/wep-2026-04-27-tir-interpreter.md` for the planned trajectory
//! (local-variable environment, `if` / `match` reduction, bounded loop
//! unrolling, pure function inlining, and a complementary wasm-CTFE
//! backend).

use std::cell::RefCell;
use std::rc::Rc;

use crate::hashmap::IndexMap;
use crate::hashmap::IndexSet;
use crate::module_source::ModuleSource;
use crate::nir::{NirBinaryOp, NirFunction, NirLiteralPattern, NirUnaryOp};
use crate::nir_arena::{
    ArmData, BlockId, BlockNode, Body, ExprId, ExprKind, ExprNode, PatId, PatKind, StmtId,
    StmtKind, StmtNode,
};
use crate::tir::{PrimitiveType, ResolvedType, TypeId, TypeTable};

/// Three-state lattice over compile-time evaluation results.
///
/// Ordering: `Unevaluated` ⊑ `Const(v)` ⊑ `NonConst`. Equivalent to the
/// classical SCCP lattice (Wegman & Zadeck, 1991): `Unevaluated` ↔
/// `Bottom`, `NonConst` ↔ `Top`. Names favour readability over the
/// academic terms — readers familiar with the abstract-interpretation
/// literature can mentally substitute Bottom/Top.
///
/// Why three states: `Option<Value>` (the previous design) collapsed
/// "I haven't computed this yet" and "I know this isn't a constant"
/// into the same `None`, which makes memoization unsound — a cached
/// `None` can't say whether a re-attempt would succeed. The lattice
/// fixes this at the type level.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Lattice {
    /// No information yet. Default for un-bound locals and NIR kinds
    /// the engine doesn't currently understand (e.g. a `Call` whose
    /// callee isn't pure-foldable, `Block` past a single tail
    /// expression).
    Unevaluated,
    /// Provably reduces to this value.
    Const(Value),
    /// Cannot be a reusable constant: a `let mut` binding, the result
    /// of a runtime-only operation (e.g. `x = …`, division by zero),
    /// or a fold whose operands are themselves `NonConst`.
    NonConst,
}

impl Lattice {
    /// Project to `Some(v)` only when the result is `Const`. The right
    /// shorthand for callers whose only question is "do you have a
    /// literal for me?" — the `Unevaluated` / `NonConst` distinction
    /// is collapsed into `None`. When that distinction matters
    /// (memoization, SCCP-style joins), pattern-match the variant
    /// directly instead of going through this projection.
    #[must_use]
    pub fn as_const(&self) -> Option<Value> {
        match self {
            Self::Const(v) => Some(*v),
            _ => None,
        }
    }

    /// Join two lattice values. This is the SCCP join (least upper bound)
    /// over the chain `Unevaluated ⊑ Const(v) ⊑ NonConst`:
    ///
    /// - `Unevaluated ⊔ x = x` — an `Unevaluated` arm is treated as
    ///   contributing no information (e.g. an `if true { … }` whose
    ///   `else` is unreachable / absent: the join with the executed arm
    ///   carries the executed arm's value out).
    /// - `Const(v) ⊔ Const(v) = Const(v)` — both arms agree, the
    ///   surrounding expression has that value regardless of which arm
    ///   ran. This is what lets `if cond { 5 } else { 5 }` collapse to
    ///   `5` when the condition is effect-free.
    /// - `Const(a) ⊔ Const(b) = NonConst` (when `a ≠ b`) — arms
    ///   disagree, the merged value is non-constant.
    /// - `NonConst ⊔ _ = NonConst` — top is absorbing.
    ///
    /// The operation is commutative, associative, and idempotent, as
    /// required of a lattice join. Used by [`Interpreter::reduce_local`]
    /// to merge the lattice of an `if` expression's two arms when the
    /// condition is non-constant.
    #[must_use]
    pub fn join(self, other: Self) -> Self {
        match (self, other) {
            (Self::Unevaluated, x) | (x, Self::Unevaluated) => x,
            (Self::NonConst, _) | (_, Self::NonConst) => Self::NonConst,
            (Self::Const(a), Self::Const(b)) if a == b => Self::Const(a),
            (Self::Const(_), Self::Const(_)) => Self::NonConst,
        }
    }
}

/// A typed compile-time value produced by the interpreter.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Value {
    /// Integer value. `prim` carries the integer type (i8..i64, u8..u64);
    /// `value` is the raw bit pattern, sign-extended for signed types.
    Int { value: u64, prim: PrimitiveType },
    /// Floating-point value. `prim` is `F32` or `F64`. For `F32`, `value`
    /// holds the f32 result widened to f64.
    Float { value: f64, prim: PrimitiveType },
    /// Boolean value.
    Bool(bool),
    /// Unicode scalar value (`char`).
    Char(char),
}

impl Value {
    /// Returns the raw integer bit pattern, or `None` if not an int.
    #[must_use]
    pub fn as_int(&self) -> Option<(u64, PrimitiveType)> {
        match self {
            Self::Int { value, prim } => Some((*value, *prim)),
            _ => None,
        }
    }

    /// Returns the raw float value and width, or `None` if not a float.
    #[must_use]
    pub fn as_float(&self) -> Option<(f64, PrimitiveType)> {
        match self {
            Self::Float { value, prim } => Some((*value, *prim)),
            _ => None,
        }
    }

    /// Returns the boolean value, or `None` if not a bool.
    #[must_use]
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// Returns the char value, or `None` if not a char.
    #[must_use]
    pub fn as_char(&self) -> Option<char> {
        match self {
            Self::Char(c) => Some(*c),
            _ => None,
        }
    }

    /// Render the value as a NIR-compatible literal repr string.
    #[must_use]
    pub fn format_repr(&self) -> String {
        match self {
            Self::Int { value, prim } => format_int_repr(*value, *prim),
            Self::Float { value, .. } => format_float_repr(*value),
            Self::Bool(b) => b.to_string(),
            Self::Char(c) => format_char_repr(*c),
        }
    }

    /// Project an arena expression to a `Value` when it's a primitive literal
    /// whose `type_id` resolves to a tracked primitive. Returns `None` for
    /// non-literal shapes (`Local`, `Call`, `Binary`, …), for `String` /
    /// `Bytes` / `Null` / `Unit` (no `Value` carrier), for the bignum
    /// primitives `i128` / `u128` (out of scope for niri folding), and for any
    /// literal whose `type_id` doesn't resolve to a primitive.
    ///
    /// Used by the const-fold visitor to turn struct-field literals
    /// (`StructLiteral { f: 5, … }`) and direct field stores (`obj.f = 5`) into
    /// `Interpreter::bind_field` / `field_env` entries.
    #[must_use]
    pub fn from_arena_literal(body: &Body, e: ExprId, type_table: &TypeTable) -> Option<Self> {
        let node = &body.exprs[e];
        match &node.kind {
            ExprKind::IntLiteral { value, .. } => {
                let prim = prim_of(node.type_id, type_table).filter(|p| is_int_prim(*p))?;
                Some(Self::Int {
                    value: *value,
                    prim,
                })
            }
            ExprKind::FloatLiteral { value, .. } => {
                let prim = prim_of(node.type_id, type_table)
                    .filter(|p| matches!(p, PrimitiveType::F32 | PrimitiveType::F64))?;
                Some(Self::Float {
                    value: *value,
                    prim,
                })
            }
            ExprKind::BoolLiteral(b) => Some(Self::Bool(*b)),
            ExprKind::CharLiteral(c) => Some(Self::Char(*c)),
            _ => None,
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Callee map
// ──────────────────────────────────────────────────────────────────────────────

/// Identity of a callee in the [`CalleeMap`]. Mirrors the shape produced
/// by `FunctionRef::full_name` so the interpreter can look up a `Call`
/// node's target without re-deriving the format.
pub type CalleeKey = (ModuleSource, String);

/// Map of CTFE-eligible callees, keyed by `(module_source, full_name)`.
///
/// Values are [`Rc<RefCell<NirFunction>>`] handles aliased with
/// [`crate::flat_package::FlatPackage::functions`], not body clones, so
/// rebuilding the map every optimizer iteration costs only refcount
/// bumps. The interpreter reads each callee via
/// [`std::cell::RefCell::try_borrow`]; the failure path catches the
/// case where the visitor is currently holding `borrow_mut` on this
/// same function (i.e. self-recursive calls inside the function being
/// walked). CTFE-internal recursion across nested folds is handled
/// separately by `Interpreter::call_stack`, since `try_borrow` permits
/// concurrent immutable borrows.
///
/// The purity / CTFE-safety gate is decided once at map construction
/// time by [`is_ctfe_eligible`], and the interpreter never re-checks
/// it. Body-shape and per-call validity (arity match, all args
/// reduce, single recognized tail expression) are checked at fold
/// time, not here.
pub type CalleeMap = IndexMap<CalleeKey, Rc<RefCell<NirFunction>>>;

// ──────────────────────────────────────────────────────────────────────────────
// Global env
// ──────────────────────────────────────────────────────────────────────────────

/// Identity of a global variable in the [`GlobalEnv`]. Mirrors the
/// `(module_source, name)` shape carried by `ExprKind::GlobalVarGet`
/// so the interpreter can look up a `GlobalVarGet` node directly.
pub type GlobalKey = (ModuleSource, String);

/// Lattice values for module-scope globals.
///
/// Populated once per pass by the driving visitor from
/// [`crate::flat_package::FlatPackage::globals`] — typically by
/// reducing each non-`mut` global's initializer through a fresh
/// [`Interpreter`] (so initializers like `1 + 2`, `i32::MAX - 1`, or
/// pure-call expressions all collapse to `Const(_)`). Mutable globals
/// are mapped to [`Lattice::NonConst`] so reads through niri stay
/// conservative even while the global is in scope.
///
/// The map is read at every `GlobalVarGet` lookup; absent keys default
/// to [`Lattice::Unevaluated`] (the engine simply doesn't know — same
/// rule as un-bound locals).
pub type GlobalEnv = IndexMap<GlobalKey, Lattice>;

/// Known constant field values of module-scope globals, keyed by global then
/// field name. The global analogue of [`Interpreter::field_env`]: it lets
/// `FieldAccess(GlobalVarGet(X), f)` fold to a constant when `X` is an
/// immutable global whose `f` field is statically known — e.g. the
/// [`SeqField::Len`](crate::compiler_item::SeqField) length of an immutable
/// sequence global hoisted by body globalization.
pub type GlobalFieldEnv = IndexMap<GlobalKey, IndexMap<String, Value>>;

/// Default per-pass CTFE step budget. Mirrors rustc's CTFE step counter
/// shape: a hard ceiling on the number of productive call entries
/// before the engine starts bailing. Borrow-blocked re-entries (the
/// recursion guard) bail before the budget charge, so they don't
/// consume budget; the ceiling only applies to new-frame work that
/// actually runs.
pub const DEFAULT_STEP_BUDGET: u32 = 1000;

// ──────────────────────────────────────────────────────────────────────────────
// Field knowledge
// ──────────────────────────────────────────────────────────────────────────────

/// A dense set of local indices, backed by a bitset indexed by the local
/// index itself.
///
/// Local indices within a function body are dense (`0..locals.len()`), so
/// this replaces an `IndexSet<u32>` used purely for membership with a
/// hash-free bitset — the same idea as [`crate::tir::TypeSet`]. The alias
/// analysis rebuilds these sets for every function on every const-fold
/// iteration, so dropping the per-grow allocation + hashing of an
/// `IndexSet` is worthwhile.
#[derive(Default, Clone, Debug)]
pub struct LocalSet {
    words: Vec<u64>,
}

impl LocalSet {
    /// An empty set pre-sized to hold `locals` indices without regrowing.
    #[must_use]
    pub fn with_capacity(locals: usize) -> Self {
        Self {
            words: vec![0; locals.div_ceil(64)],
        }
    }

    fn slot(index: u32) -> (usize, u64) {
        ((index / 64) as usize, 1u64 << (index % 64))
    }

    /// Insert `index`, returning `true` if it was not already present.
    pub fn insert(&mut self, index: u32) -> bool {
        let (word, mask) = Self::slot(index);
        if word >= self.words.len() {
            self.words.resize(word + 1, 0);
        }
        let newly = self.words[word] & mask == 0;
        self.words[word] |= mask;
        newly
    }

    /// Whether `index` is a member.
    #[must_use]
    pub fn contains(&self, index: u32) -> bool {
        let (word, mask) = Self::slot(index);
        self.words.get(word).is_some_and(|w| w & mask != 0)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.words.iter().all(|&w| w == 0)
    }

    /// Iterate members in ascending index order.
    pub fn iter(&self) -> impl Iterator<Item = u32> + '_ {
        self.words.iter().enumerate().flat_map(|(wi, &word)| {
            (0..64u32)
                .filter(move |&b| word & (1u64 << b) != 0)
                .map(move |b| wi as u32 * 64 + b)
        })
    }
}

/// Per-function alias / aliasing-trackability annotations consumed by
/// the interpreter's field-knowledge bookkeeping.
///
/// These three sets are computed once per function by the driving
/// visitor (typically from the function's stable
/// `address_taken_locals` / `stores_aliased_locals` plus a body walk
/// that catches transient inlined-in copies), then handed to the
/// interpreter via [`Interpreter::set_alias_info`].
///
/// - `aliased`: locals reachable through some other handle (`&x`,
///   `&mut x`, captured by a closure, struct-field-stored, etc.).
///   Field knowledge IS recorded for these locals; the flow-sensitive
///   walk drops their entries at every side-effect boundary (call,
///   dereferenced write, …) where an unseen alias could have mutated
///   the storage.
/// - `untrackable`: locals whose aliasing escapes our analysis (e.g.
///   stashed across a `stores`-annotated callee). Field knowledge is
///   **never** recorded for these; that matches the conservatism the
///   OLD WIR-level `const_forward` had for stores-passed args.
/// - `alias_groups`: union-find groups of locals connected by
///   reference-typed `let dst = src` copies (`Box<T>`, `List<T>`,
///   `&T`, `&mut T`). Used to widen field-assignment invalidation:
///   writing `dst.field = …` must drop the same field on every
///   alias.
#[derive(Default, Clone, Debug)]
pub struct AliasInfo {
    pub aliased: LocalSet,
    pub untrackable: LocalSet,
    pub alias_groups: IndexMap<u32, IndexSet<u32>>,
}

/// Snapshot of [`Interpreter::field_env`] returned by
/// [`Interpreter::snapshot_fields`]. Restored verbatim by
/// [`Interpreter::restore_fields`]; used by the driving visitor to
/// fork field knowledge at branch boundaries (`if`, `match`, `if let`)
/// so each arm walks against the entry state.
#[derive(Clone, Debug)]
pub struct FieldSnapshot {
    fields: IndexMap<u32, IndexMap<String, Value>>,
}

impl FieldSnapshot {
    /// Empty snapshot — no bindings. Conceptually the bottom element
    /// of the field-env lattice; meeting with anything else discards
    /// every binding.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            fields: IndexMap::default(),
        }
    }

    /// Lattice meet: keep only `(local, field) → value` entries that
    /// appear in **both** snapshots bound to the **same** value.
    /// Entries present in only one snapshot, or bound to different
    /// values, are dropped.
    ///
    /// Used by the driving visitor at if-stmt / match / switch
    /// boundaries to compute the post-branch state as the join of the
    /// per-arm post-states. The semantics mirror the standard
    /// dataflow lattice join: a fact holds after the branch iff it
    /// holds on every reachable arm with the same value.
    #[must_use]
    pub fn meet(self, other: &Self) -> Self {
        let mut out: IndexMap<u32, IndexMap<String, Value>> = IndexMap::default();
        for (local, my_fields) in self.fields {
            let Some(other_fields) = other.fields.get(&local) else {
                continue;
            };
            let mut merged: IndexMap<String, Value> = IndexMap::default();
            for (name, val) in my_fields {
                if other_fields.get(&name) == Some(&val) {
                    merged.insert(name, val);
                }
            }
            if !merged.is_empty() {
                out.insert(local, merged);
            }
        }
        Self { fields: out }
    }
}

/// One arm of a branch (`if` then / else, `match` arm, `switch`
/// arm) joining into [`FieldSnapshot::join_arms`]. An arm with
/// `reachable = false` terminates (`return` / `break` / `continue`
/// / `panic(…)` / call returning `!`) and is excluded from the meet.
#[derive(Clone, Debug)]
pub struct Arm {
    pub reachable: bool,
    pub post_state: FieldSnapshot,
}

impl FieldSnapshot {
    /// Lattice meet of every reachable arm's post-state.
    /// Unreachable arms (`reachable = false`) are excluded — their
    /// writes are not observed past the branch. If no arm is
    /// reachable, the post-branch point is itself dead code and
    /// `snap_pre` is returned as an arbitrary placeholder.
    ///
    /// Callers model an implicit no-`else` arm as a reachable arm
    /// carrying `snap_pre`.
    #[must_use]
    pub fn join_arms(snap_pre: FieldSnapshot, arms: impl IntoIterator<Item = Arm>) -> Self {
        let mut accumulator: Option<FieldSnapshot> = None;
        for arm in arms {
            if !arm.reachable {
                continue;
            }
            accumulator = Some(match accumulator {
                None => arm.post_state,
                Some(acc) => acc.meet(&arm.post_state),
            });
        }
        accumulator.unwrap_or(snap_pre)
    }
}

#[cfg(test)]
mod field_snapshot_tests {
    use super::{FieldSnapshot, PrimitiveType, Value};
    use crate::hashmap::IndexMap;

    fn int(v: u64) -> Value {
        Value::Int {
            value: v,
            prim: PrimitiveType::I32,
        }
    }

    fn snap(entries: &[(u32, &[(&str, Value)])]) -> FieldSnapshot {
        let mut fields: IndexMap<u32, IndexMap<String, Value>> = IndexMap::default();
        for (local, kvs) in entries {
            let mut m: IndexMap<String, Value> = IndexMap::default();
            for (k, v) in *kvs {
                m.insert((*k).to_string(), *v);
            }
            fields.insert(*local, m);
        }
        FieldSnapshot { fields }
    }

    fn extract(s: &FieldSnapshot) -> Vec<(u32, Vec<(String, Value)>)> {
        s.fields
            .iter()
            .map(|(l, m)| {
                let mut v: Vec<_> = m.iter().map(|(k, v)| (k.clone(), *v)).collect();
                v.sort_by(|a, b| a.0.cmp(&b.0));
                (*l, v)
            })
            .collect()
    }

    #[test]
    fn meet_with_empty_is_empty() {
        let a = snap(&[(0, &[("used", int(16))])]);
        let empty = FieldSnapshot::empty();
        assert_eq!(extract(&a.clone().meet(&empty)), vec![]);
        assert_eq!(extract(&empty.meet(&a)), vec![]);
    }

    #[test]
    fn meet_with_self_is_self() {
        let a = snap(&[(0, &[("used", int(16)), ("cap", int(32))])]);
        let result = a.clone().meet(&a);
        assert_eq!(extract(&result), extract(&a));
    }

    #[test]
    fn meet_keeps_matching_fields_drops_mismatched() {
        let a = snap(&[(0, &[("used", int(16)), ("cap", int(32))])]);
        let b = snap(&[(0, &[("used", int(16)), ("cap", int(64))])]);
        // `used` agrees → kept; `cap` disagrees → dropped; local
        // survives because at least one field agrees.
        let result = a.meet(&b);
        assert_eq!(
            extract(&result),
            vec![(0, vec![("used".to_string(), int(16))])]
        );
    }

    #[test]
    fn meet_drops_locals_only_present_in_one() {
        let a = snap(&[(0, &[("used", int(16))]), (1, &[("size", int(8))])]);
        let b = snap(&[(0, &[("used", int(16))])]);
        // Local 1 is missing from `b` → dropped from meet.
        let result = a.meet(&b);
        assert_eq!(
            extract(&result),
            vec![(0, vec![("used".to_string(), int(16))])]
        );
    }

    #[test]
    fn meet_drops_local_when_no_field_agrees() {
        let a = snap(&[(0, &[("used", int(16))])]);
        let b = snap(&[(0, &[("used", int(32))])]);
        // Single field disagrees → entire local dropped.
        let result = a.meet(&b);
        assert_eq!(extract(&result), vec![]);
    }

    fn arm(reachable: bool, post: FieldSnapshot) -> super::Arm {
        super::Arm {
            reachable,
            post_state: post,
        }
    }

    #[test]
    fn join_arms_zero_reachable_returns_pre() {
        let pre = snap(&[(0, &[("used", int(99))])]);
        let result = FieldSnapshot::join_arms(
            pre.clone(),
            vec![
                arm(false, FieldSnapshot::empty()),
                arm(false, FieldSnapshot::empty()),
            ],
        );
        assert_eq!(extract(&result), extract(&pre));
    }

    #[test]
    fn join_arms_single_reachable_is_that_arms_post_state() {
        let pre = snap(&[(0, &[("used", int(99))])]);
        let then = snap(&[(0, &[("used", int(16))])]);
        let result = FieldSnapshot::join_arms(
            pre,
            vec![arm(true, then.clone()), arm(false, FieldSnapshot::empty())],
        );
        assert_eq!(extract(&result), extract(&then));
    }

    #[test]
    fn join_arms_multiple_reachable_meets_them() {
        let pre = FieldSnapshot::empty();
        let then = snap(&[(0, &[("used", int(16)), ("cap", int(32))])]);
        let els = snap(&[(0, &[("used", int(16)), ("cap", int(64))])]);
        let result = FieldSnapshot::join_arms(pre, vec![arm(true, then), arm(true, els)]);
        // `used` agrees → kept; `cap` disagrees → dropped.
        assert_eq!(
            extract(&result),
            vec![(0, vec![("used".to_string(), int(16))])]
        );
    }

    #[test]
    fn join_arms_implicit_else_is_pre_as_a_reachable_arm() {
        // No-else if encoded as `[then, Arm { reachable: true, snap_pre }]`.
        let pre = snap(&[(0, &[("used", int(16))])]);
        let then = snap(&[(0, &[("used", int(16)), ("cap", int(99))])]);
        let result = FieldSnapshot::join_arms(pre.clone(), vec![arm(true, then), arm(true, pre)]);
        // Pre's lack-of-cap drops the then-arm's cap; `used = 16` agrees.
        assert_eq!(
            extract(&result),
            vec![(0, vec![("used".to_string(), int(16))])]
        );
    }
}

/// Decide whether a function may be evaluated at compile time.
///
/// The check is a conservative pure-and-safe gate, applied once when the
/// driving visitor builds the [`CalleeMap`]:
///
/// - `effects.is_empty()` — no `with` clauses (the effect system's purity
///   witness, modulo trap effects which Wado tracks separately).
/// - `body.is_some()` — has a Wado-source body. External / CM-import
///   functions have no inspectable body.
/// - `!is_cm_binding && !is_dispatch_wrapper && !is_cm_export` —
///   synthesized ABI bridges aren't real Wado functions.
/// - `!is_async && task_return_type.is_none()` — async functions
///   participate in the CM async runtime; not CTFE-safe.
/// - `stores.is_empty()` — `stores[...]` is moot for CTFE (we don't pass
///   refs), but bail conservatively.
/// - `inline_hint != InlineHint::Never` — respect the user's explicit
///   "do not expand this" annotation.
/// - `type_params` and `impl_type_params` empty — CTFE runs after
///   monomorphization, so concrete bodies only.
#[must_use]
pub fn is_ctfe_eligible(func: &NirFunction) -> bool {
    func.effects.is_empty()
        && func.body.is_some()
        && !func.is_cm_binding
        && !func.is_dispatch_wrapper
        && !func.is_cm_export
        && !func.is_async
        && func.task_return_type.is_none()
        && func.stores.is_empty()
        && func.inline_hint != crate::nir::InlineHint::Never
        && func.type_params.is_empty()
        && func.impl_type_params.is_empty()
}

// ──────────────────────────────────────────────────────────────────────────────
// Interpreter
// ──────────────────────────────────────────────────────────────────────────────

/// Partial evaluator over the arena `Body`.
///
/// Holds the type table needed to resolve operand widths, a per-function
/// `env` mapping local indices to lattice values, an optional
/// [`CalleeMap`] of pure-eligible callees, a step budget, and a
/// `call_stack` of in-flight CTFE frames for recursion detection.
pub struct Interpreter<'a> {
    type_table: &'a TypeTable,
    /// Lattice values for `let`-bound locals in the *current function*.
    /// Populated by the driving visitor via [`bind_local`] /
    /// [`invalidate_local`]; cleared via [`enter_function`]. Reads of
    /// `ExprKind::Local` consult this map during folding.
    ///
    /// Locals not present in the map default to [`Lattice::Unevaluated`].
    ///
    /// [`bind_local`]: Self::bind_local
    /// [`invalidate_local`]: Self::invalidate_local
    /// [`enter_function`]: Self::enter_function
    env: IndexMap<u32, Lattice>,
    /// Per-(local, field) constant values for the *current function*.
    ///
    /// Populated by the driving visitor when it sees a `let local =
    /// StructLiteral { f: lit, … }`, a `local.field = lit` assignment,
    /// or a recognized `$value_copy$T(src)` / Local→Local copy that
    /// transfers field knowledge. Reads at `FieldAccess(Local(idx),
    /// field_name)` sites consult this map and rewrite the read to the
    /// recorded literal.
    ///
    /// Only the four primitive literal kinds (Int / Float / Bool /
    /// Char) — exactly the values [`Value`] models — are forwardable;
    /// `String` / `null` / aggregate fields stay un-recorded so their
    /// reads always go through the runtime.
    ///
    /// Stored as a nested `IndexMap<local_index, IndexMap<field_name,
    /// Value>>` so the lookup path (every `FieldAccess(Local, _)`
    /// read in the program) can probe with a borrowed `&str` field
    /// name — no `String` allocation per read. Per-local
    /// invalidation (`invalidate_local`,
    /// `invalidate_aliased_fields`) collapses to an `O(1)`
    /// `swap_remove` on the outer map for each affected local
    /// instead of an `O(n_fields)` `retain` over a flat key set.
    field_env: IndexMap<u32, IndexMap<String, Value>>,
    /// Per-function alias annotations driving [`field_env`]
    /// invalidation. Empty by default; populated once per function by
    /// the driving visitor via [`set_alias_info`].
    ///
    /// [`field_env`]: Self::field_env
    /// [`set_alias_info`]: Self::set_alias_info
    alias_info: AliasInfo,
    /// Pre-built map of CTFE-eligible callees. When `None`, `Call` nodes
    /// stay [`Lattice::Unevaluated`]. The visitor populates this once
    /// per pass via [`with_callees`].
    ///
    /// [`with_callees`]: Self::with_callees
    callees: Option<&'a CalleeMap>,
    /// Pre-built lattice values for module-scope globals. When `None`,
    /// every `GlobalVarGet` stays [`Lattice::Unevaluated`]. The visitor
    /// populates this once per pass via [`with_globals`].
    ///
    /// [`with_globals`]: Self::with_globals
    globals: Option<&'a GlobalEnv>,
    /// Pre-built constant field values of module-scope globals. When `None`,
    /// `FieldAccess(GlobalVarGet(_), _)` stays [`Lattice::Unevaluated`]. The
    /// visitor populates this once per pass via [`with_global_fields`].
    ///
    /// [`with_global_fields`]: Self::with_global_fields
    global_fields: Option<&'a GlobalFieldEnv>,
    /// Hard ceiling on the number of productive CTFE call entries
    /// before bailing. Decremented once per successful body evaluation;
    /// on zero, further attempts return `Unevaluated`.
    step_budget: u32,
    /// Keys of the callees whose bodies the engine is currently
    /// evaluating, in entry order. A `Call` to a key already on the
    /// stack reports `Unevaluated` immediately, so direct (`f → f`)
    /// and indirect (`f → g → f`) recursion terminate without
    /// consuming budget. The `RefCell` borrow guard cannot serve this
    /// role on its own because `try_borrow` permits concurrent
    /// immutable borrows.
    call_stack: Vec<CalleeKey>,
}

impl<'a> Interpreter<'a> {
    #[must_use]
    pub fn new(type_table: &'a TypeTable) -> Self {
        Self {
            type_table,
            env: IndexMap::default(),
            field_env: IndexMap::default(),
            alias_info: AliasInfo::default(),
            callees: None,
            globals: None,
            global_fields: None,
            step_budget: DEFAULT_STEP_BUDGET,
            call_stack: Vec::new(),
        }
    }

    /// Install the [`CalleeMap`]. Without this, every `Call` node
    /// remains [`Lattice::Unevaluated`] — the engine has no body to
    /// look up.
    ///
    /// Lifetime: the map outlives this interpreter (the visitor builds
    /// it once per pass, hands a borrow in, and discards both at end of
    /// pass).
    pub fn with_callees(&mut self, callees: &'a CalleeMap) -> &mut Self {
        self.callees = Some(callees);
        self
    }

    /// Install the [`GlobalEnv`]. Without this, every `GlobalVarGet`
    /// node remains [`Lattice::Unevaluated`] — the engine has no
    /// initializer lattice to look up.
    ///
    /// Lifetime: the map outlives this interpreter (the visitor builds
    /// it once per pass, hands a borrow in, and discards both at end of
    /// pass), mirroring [`with_callees`].
    ///
    /// [`with_callees`]: Self::with_callees
    pub fn with_globals(&mut self, globals: &'a GlobalEnv) -> &mut Self {
        self.globals = Some(globals);
        self
    }

    /// Install the [`GlobalFieldEnv`]. Without this,
    /// `FieldAccess(GlobalVarGet(_), _)` stays [`Lattice::Unevaluated`].
    /// Mirrors [`with_globals`](Self::with_globals).
    pub fn with_global_fields(&mut self, global_fields: &'a GlobalFieldEnv) -> &mut Self {
        self.global_fields = Some(global_fields);
        self
    }

    /// Override the per-pass CTFE step budget (default
    /// [`DEFAULT_STEP_BUDGET`]). Called rarely — primarily by tests
    /// exercising the budget-exhaustion path.
    pub fn set_step_budget(&mut self, budget: u32) -> &mut Self {
        self.step_budget = budget;
        self
    }

    /// Reset the per-function environment. The driving visitor must call
    /// this before walking each function body; otherwise a previous
    /// function's bindings would leak into the next one (local indices
    /// are unique per function, not project-wide).
    ///
    /// Asserts the recursion guard is clear — a leaked entry would mean
    /// a previous walk panicked mid-call. The step budget is
    /// intentionally *not* touched: it caps total CTFE work across the
    /// pass, not per-function.
    pub fn enter_function(&mut self) {
        self.env.clear();
        self.field_env.clear();
        self.alias_info = AliasInfo::default();
        debug_assert!(
            self.call_stack.is_empty(),
            "niri call_stack leaked across function boundary",
        );
    }

    /// Install per-function alias annotations. The driving visitor
    /// calls this after [`enter_function`] and before walking the
    /// body. See [`AliasInfo`] for the meaning of each set.
    ///
    /// [`enter_function`]: Self::enter_function
    pub fn set_alias_info(&mut self, info: AliasInfo) {
        self.alias_info = info;
    }

    /// Read-only borrow of the currently-installed `aliased` set.
    /// The const-fold visitor needs this to decide whether an
    /// expression appearing as a struct / tuple / variant field
    /// value captures access to an already-aliased local — a
    /// condition that, after the constructor runs, has to invalidate
    /// every aliased local's recorded fields.
    #[must_use]
    pub fn aliased_locals(&self) -> &LocalSet {
        &self.alias_info.aliased
    }

    /// Record a lattice value for a `let`-bound local. The driving
    /// visitor calls this after walking a `Let` statement: pass
    /// [`Lattice::Const`] for an immutable binding whose RHS reduced,
    /// [`Lattice::NonConst`] for `let mut` or any RHS that could not be
    /// reduced.
    pub fn bind_local(&mut self, index: u32, lattice: Lattice) {
        self.env.insert(index, lattice);
    }

    /// Mark a local as definitely non-constant from this point on. The
    /// driving visitor calls this when it sees an `x = expr` assignment.
    /// Conservative — we don't track flow-sensitive new values, just
    /// invalidate the prior binding. Also drops every recorded field
    /// of the local — the new value might not have those fields, or
    /// might have different ones.
    pub fn invalidate_local(&mut self, index: u32) {
        self.env.insert(index, Lattice::NonConst);
        self.field_env.swap_remove(&index);
    }

    /// Record `value` as the known compile-time value of
    /// `local_index.field_name`. The driving visitor calls this when
    /// it sees a `let local = StructLiteral { field_name: lit, … }`,
    /// a direct `local.field_name = lit` assignment, or a recognized
    /// field-knowledge transfer (`$value_copy$T(src)` /
    /// reference-typed `let dst = src`). Reads at `FieldAccess(Local,
    /// field_name)` consult the recorded value through
    /// [`expr_to_lattice`] / [`reduce_local`].
    ///
    /// Skipped silently when `local_index` is in the `untrackable`
    /// set — those locals participate in aliasing the optimizer can
    /// no longer see, so any later read may witness a mutation we
    /// never recorded. (Same conservatism as the WIR-level
    /// `const_forward` had for stores-passed args.)
    ///
    /// [`expr_to_lattice`]: Self::expr_to_lattice
    /// [`reduce_local`]: Self::reduce_local
    pub fn bind_field(&mut self, local_index: u32, field_name: &str, value: Value) {
        if self.alias_info.untrackable.contains(local_index) {
            return;
        }
        self.field_env
            .entry(local_index)
            .or_default()
            .insert(field_name.to_string(), value);
    }

    /// Drop the recorded value (if any) for `local_index.field_name`.
    /// The driving visitor calls this on `local.field = expr`
    /// assignments before optionally re-recording with [`bind_field`]
    /// when `expr` is a forwardable literal.
    ///
    /// Aliased locals in the same `alias_groups` entry are
    /// invalidated for the same field, since they share the
    /// underlying object's storage.
    ///
    /// [`bind_field`]: Self::bind_field
    pub fn invalidate_field(&mut self, local_index: u32, field_name: &str) {
        if let Some(m) = self.field_env.get_mut(&local_index) {
            m.swap_remove(field_name);
        }
        // Disjoint-field borrow: `&self.alias_info.alias_groups` and
        // `&mut self.field_env` access different fields of `Self`,
        // so the borrow checker accepts holding the immutable group
        // borrow across the mutable map probe.
        if let Some(group) = self.alias_info.alias_groups.get(&local_index) {
            for other in group {
                if *other == local_index {
                    continue;
                }
                if let Some(m) = self.field_env.get_mut(other) {
                    m.swap_remove(field_name);
                }
            }
        }
    }

    /// Drop every field entry whose owning local is in
    /// `alias_info.aliased`. The driving visitor calls this at
    /// side-effect boundaries (calls, dereferenced writes) where some
    /// external code could have mutated the storage through an alias.
    pub fn invalidate_aliased_fields(&mut self) {
        // Walking the (typically small) `aliased` set and probing
        // the (typically larger) `field_env` outer map by `swap_remove`
        // is O(n_aliased) — strictly better than O(n_field_env)
        // `retain` over the flat key set. When `field_env` is empty
        // (the common case for functions that don't construct
        // tracked structs) the loop body is a no-op anyway.
        if self.alias_info.aliased.is_empty() || self.field_env.is_empty() {
            return;
        }
        for idx in self.alias_info.aliased.iter() {
            self.field_env.swap_remove(&idx);
        }
    }

    /// Copy every recorded field of `src` to `dst`. Used by the
    /// driving visitor to thread field knowledge through `let dst =
    /// src` (reference-typed Local→Local copy, where both names alias
    /// the same heap object) and `let dst = $value_copy$T(src)`
    /// (the synthesized one-level shallow value-copy helper from
    /// `lower::plan::value_copy::synthesize` — field-by-field projection
    /// plus `array_clone` for raw arrays). Only primitive-literal
    /// fields are recorded in `field_env`, so for the values we
    /// actually transfer, src and dst observe the same constants
    /// regardless of the helper's depth. Skipped when `dst` is
    /// `untrackable`. Existing entries on `dst` for fields also
    /// present on `src` are overwritten with `src`'s values (src
    /// wins); fields present only on `dst` are preserved.
    pub fn copy_fields_from(&mut self, src: u32, dst: u32) {
        if src == dst || self.alias_info.untrackable.contains(dst) {
            return;
        }
        // Collect from a *borrowed* `src` map into a flat Vec so the
        // immutable borrow on `field_env` is released before we take
        // the mutable `entry(dst)`. Cloning into a Vec is cheaper
        // than cloning the whole inner `IndexMap` (no hash-table
        // copy) and skips both the index-table clone and the temporary
        // map's drop. Empty `src` short-circuits without an alloc.
        let Some(src_map) = self.field_env.get(&src) else {
            return;
        };
        if src_map.is_empty() {
            return;
        }
        let copies: Vec<(String, Value)> =
            src_map.iter().map(|(name, v)| (name.clone(), *v)).collect();
        let dst_map = self.field_env.entry(dst).or_default();
        for (name, v) in copies {
            dst_map.insert(name, v);
        }
    }

    /// Take a snapshot of the current field environment. Used by the
    /// driving visitor to fork at branch boundaries: snapshot, walk
    /// one arm, restore, walk the other. Locals don't need this fork
    /// (the only mutation channel is `let mut`, recorded preemptively
    /// as `NonConst`); fields do, because `local.field = …` inside a
    /// branch is conditional on the branch firing.
    #[must_use]
    pub fn snapshot_fields(&self) -> FieldSnapshot {
        FieldSnapshot {
            fields: self.field_env.clone(),
        }
    }

    /// Restore a [`FieldSnapshot`] taken via [`snapshot_fields`].
    ///
    /// [`snapshot_fields`]: Self::snapshot_fields
    pub fn restore_fields(&mut self, snap: FieldSnapshot) {
        self.field_env = snap.fields;
    }

    /// Drop every recorded field. Used at control-flow merges where
    /// conservatively forgetting all fields is simpler than computing
    /// the meet of per-branch knowledge.
    pub fn clear_fields(&mut self) {
        self.field_env.clear();
    }

    /// Reads the bound env for locals/fields and takes the SCCP join over
    /// `if` / `match` arms.
    pub fn expr_to_lattice_a(&self, body: &Body, e: ExprId) -> Lattice {
        let node = &body.exprs[e];
        match &node.kind {
            ExprKind::BoolLiteral(b) => Lattice::Const(Value::Bool(*b)),
            ExprKind::CharLiteral(c) => Lattice::Const(Value::Char(*c)),
            ExprKind::IntLiteral { value, .. } => {
                let Some(prim) = prim_of(node.type_id, self.type_table).filter(|p| is_int_prim(*p))
                else {
                    return Lattice::Unevaluated;
                };
                Lattice::Const(Value::Int {
                    value: *value,
                    prim,
                })
            }
            ExprKind::FloatLiteral { value, .. } => {
                let prim = if is_f32_type(node.type_id, self.type_table) {
                    PrimitiveType::F32
                } else {
                    PrimitiveType::F64
                };
                Lattice::Const(Value::Float {
                    value: *value,
                    prim,
                })
            }
            ExprKind::Local { index, .. } => {
                self.env.get(index).copied().unwrap_or(Lattice::Unevaluated)
            }
            ExprKind::FieldAccess {
                expr: inner,
                field_name,
                ..
            } => match &body.exprs[*inner].kind {
                ExprKind::Local { index, .. } => self
                    .field_env
                    .get(index)
                    .and_then(|m| m.get(field_name.as_str()))
                    .copied()
                    .map_or(Lattice::Unevaluated, Lattice::Const),
                ExprKind::GlobalVarGet {
                    module_source,
                    name,
                } => self
                    .global_fields
                    .and_then(|m| m.get(&(module_source.clone(), name.clone())))
                    .and_then(|m| m.get(field_name.as_str()))
                    .copied()
                    .map_or(Lattice::Unevaluated, Lattice::Const),
                _ => Lattice::Unevaluated,
            },
            ExprKind::GlobalVarGet {
                module_source,
                name,
            } => self.global_lattice(module_source, name),
            ExprKind::Block(b) => self.block_lattice_a(body, *b),
            ExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                let cond = self.expr_to_lattice_a(body, *condition);
                match cond {
                    Lattice::Const(Value::Bool(true)) => self.block_lattice_a(body, *then_branch),
                    Lattice::Const(Value::Bool(false)) => match else_branch {
                        Some(eb) => self.block_lattice_a(body, *eb),
                        None => Lattice::Unevaluated,
                    },
                    _ => {
                        let then_lat =
                            arm_lattice_for_feasible_join(self.block_lattice_a(body, *then_branch));
                        let else_lat = match else_branch {
                            Some(eb) => {
                                arm_lattice_for_feasible_join(self.block_lattice_a(body, *eb))
                            }
                            None => Lattice::NonConst,
                        };
                        then_lat.join(else_lat)
                    }
                }
            }
            ExprKind::Match {
                expr: scrutinee,
                arms,
            } => self.match_lattice_a(body, *scrutinee, arms),
            _ => Lattice::Unevaluated,
        }
    }

    /// Fold a `Binary` / `Unary` / `Cast` of constant operands to a value;
    /// `NonConst` (not `Unevaluated`) when the op would trap, so the node survives.
    pub fn try_fold_a(&self, body: &Body, e: ExprId) -> Lattice {
        let node = &body.exprs[e];
        match &node.kind {
            ExprKind::Binary { left, op, right } => {
                let l = match self.expr_to_lattice_a(body, *left) {
                    Lattice::Const(v) => v,
                    other => return other,
                };
                let r = match self.expr_to_lattice_a(body, *right) {
                    Lattice::Const(v) => v,
                    other => return other,
                };
                option_to_lattice(eval_binary(l, *op, r))
            }
            ExprKind::Unary { op, expr: inner } => {
                let v = match self.expr_to_lattice_a(body, *inner) {
                    Lattice::Const(v) => v,
                    other => return other,
                };
                option_to_lattice(eval_unary(*op, v))
            }
            ExprKind::Cast { expr: inner, .. } => {
                let Some(target) = prim_of(node.type_id, self.type_table) else {
                    return Lattice::Unevaluated;
                };
                match self.expr_to_lattice_a(body, *inner) {
                    Lattice::Const(v) => option_to_lattice(eval_cast(v, target)),
                    other => other,
                }
            }
            _ => Lattice::Unevaluated,
        }
    }

    /// The lattice of a block: its single tail `Expr`, else `Unevaluated`.
    fn block_lattice_a(&self, body: &Body, b: BlockId) -> Lattice {
        match body.blocks[b].stmts.as_slice() {
            [] => Lattice::Unevaluated,
            [single] => match &body.stmts[*single].kind {
                StmtKind::Expr(e) => self.expr_to_lattice_a(body, *e),
                _ => Lattice::Unevaluated,
            },
            _ => Lattice::Unevaluated,
        }
    }

    /// The lattice of a `match`: the chosen arm under a constant scrutinee,
    /// else the join over the feasible arms.
    fn match_lattice_a(&self, body: &Body, scrutinee: ExprId, arms: &[ArmData]) -> Lattice {
        let scrut_const = self.expr_to_lattice_a(body, scrutinee).as_const();
        if arms.is_empty() {
            return Lattice::Unevaluated;
        }
        if let Some(scrut_v) = scrut_const {
            let mut candidates = Vec::<Lattice>::new();
            let mut yes_found = false;
            for arm in arms {
                let pm = if arm.guard.is_some() {
                    PatternMatch::Unknown
                } else {
                    self.pattern_matches_a(body, &scrut_v, arm.pattern)
                };
                let body_lat =
                    arm_lattice_for_feasible_join(self.expr_to_lattice_a(body, arm.body));
                match pm {
                    PatternMatch::No => {}
                    PatternMatch::Yes => {
                        if candidates.is_empty() {
                            return self.expr_to_lattice_a(body, arm.body);
                        }
                        candidates.push(body_lat);
                        yes_found = true;
                        break;
                    }
                    PatternMatch::Unknown => candidates.push(body_lat),
                }
            }
            if !yes_found {
                return Lattice::NonConst;
            }
            join_all(&candidates)
        } else {
            if !is_provably_exhaustive_a(body, arms) {
                return Lattice::NonConst;
            }
            let mut acc = Lattice::Unevaluated;
            for arm in arms {
                acc = acc.join(arm_lattice_for_feasible_join(
                    self.expr_to_lattice_a(body, arm.body),
                ));
            }
            acc
        }
    }

    fn pattern_matches_a(&self, body: &Body, value: &Value, pat: PatId) -> PatternMatch {
        match &body.pats[pat].kind {
            PatKind::Wildcard => PatternMatch::Yes,
            PatKind::Literal(lit) => match (lit, value) {
                (NirLiteralPattern::I128(p), Value::Int { value: v, prim }) => {
                    bool_to_match(int_value_matches_i128(*v, *prim, *p))
                }
                (NirLiteralPattern::U128(p), Value::Int { value: v, prim }) => {
                    bool_to_match(int_value_matches_u128(*v, *prim, *p))
                }
                (NirLiteralPattern::Bool(p), Value::Bool(v)) => bool_to_match(p == v),
                (NirLiteralPattern::Char(p), Value::Char(v)) => bool_to_match(p == v),
                (
                    NirLiteralPattern::I128(_)
                    | NirLiteralPattern::U128(_)
                    | NirLiteralPattern::Bool(_)
                    | NirLiteralPattern::Char(_),
                    _,
                ) => PatternMatch::No,
                (NirLiteralPattern::String(_) | NirLiteralPattern::Null, _) => {
                    PatternMatch::Unknown
                }
            },
            PatKind::Or(alts) => {
                let mut any_unknown = false;
                for alt in alts {
                    match self.pattern_matches_a(body, value, *alt) {
                        PatternMatch::Yes => return PatternMatch::Yes,
                        PatternMatch::No => {}
                        PatternMatch::Unknown => any_unknown = true,
                    }
                }
                if any_unknown {
                    PatternMatch::Unknown
                } else {
                    PatternMatch::No
                }
            }
            PatKind::Range {
                start,
                end,
                inclusive,
                is_unsigned,
            } => match value {
                Value::Int { value: v, prim } => bool_to_match(range_matches_int(
                    *v,
                    *prim,
                    *start,
                    *end,
                    *inclusive,
                    *is_unsigned,
                )),
                Value::Char(c) => {
                    let cp = i128::from(u32::from(*c));
                    bool_to_match(if *inclusive {
                        cp >= *start && cp <= *end
                    } else {
                        cp >= *start && cp < *end
                    })
                }
                _ => PatternMatch::No,
            },
            PatKind::ConstantValue { expr } => {
                match self.expr_to_lattice_a(body, *expr).as_const() {
                    Some(v) if &v == value => PatternMatch::Yes,
                    Some(_) => PatternMatch::No,
                    None => PatternMatch::Unknown,
                }
            }
            PatKind::Binding { .. }
            | PatKind::Tuple(_, _)
            | PatKind::Variant { .. }
            | PatKind::Enum { .. }
            | PatKind::Struct { .. } => PatternMatch::Unknown,
        }
    }

    // ───────────────────────────────────────────────────────────────────────
    // Arena rewriter. The arena counterparts of `reduce_local` /
    // `reduce_local_block` / `rewrite_if_expr` / `rewrite_match_expr` /
    // `try_call_fold`, mutating the `Body` the const-fold visitor walks.
    // ───────────────────────────────────────────────────────────────────────

    /// The single-node rewrites at `e` (no recursion into children).
    pub fn reduce_local_a(&mut self, body: &mut Body, e: ExprId) -> bool {
        if let Lattice::Const(v) = self.try_fold_a(body, e) {
            body.exprs[e].kind = value_to_arena_kind(v);
            return true;
        }
        // GlobalVarGet → recorded Const.
        let global_v = if let ExprKind::GlobalVarGet {
            module_source,
            name,
        } = &body.exprs[e].kind
        {
            match self.global_lattice(module_source, name) {
                Lattice::Const(v) => Some(v),
                _ => None,
            }
        } else {
            None
        };
        if let Some(v) = global_v {
            body.exprs[e].kind = value_to_arena_kind(v);
            return true;
        }
        // FieldAccess(Local, field) → field_env Const.
        let field_v = if let ExprKind::FieldAccess {
            expr: inner,
            field_name,
            ..
        } = &body.exprs[e].kind
        {
            if let ExprKind::Local { index, .. } = &body.exprs[*inner].kind {
                self.field_env
                    .get(index)
                    .and_then(|m| m.get(field_name.as_str()))
                    .copied()
            } else {
                None
            }
        } else {
            None
        };
        if let Some(v) = field_v {
            body.exprs[e].kind = value_to_arena_kind(v);
            return true;
        }
        if let Lattice::Const(v) = self.try_call_fold_a(body, e) {
            body.exprs[e].kind = value_to_arena_kind(v);
            return true;
        }
        if rewrite_short_circuit_a(body, e) {
            return true;
        }
        if self.rewrite_if_expr_a(body, e) {
            return true;
        }
        self.rewrite_match_expr_a(body, e)
    }

    /// Splice a constant-condition `if` statement into its parent block.
    pub fn reduce_local_block_a(&mut self, body: &mut Body, block: BlockId) -> bool {
        let has_constant_if = body.blocks[block].stmts.iter().any(|s| {
            matches!(
                &body.stmts[*s].kind,
                StmtKind::If { condition, .. }
                    if matches!(body.exprs[*condition].kind, ExprKind::BoolLiteral(_))
            )
        });
        if !has_constant_if {
            return false;
        }
        let old_stmts = std::mem::take(&mut body.blocks[block].stmts);
        let mut new_stmts: Vec<crate::nir_arena::StmtId> = Vec::new();
        for s in old_stmts {
            let spliced = if let StmtKind::If {
                condition,
                then_block,
                else_block,
            } = &body.stmts[s].kind
            {
                if let ExprKind::BoolLiteral(value) = body.exprs[*condition].kind {
                    Some((value, *then_block, *else_block))
                } else {
                    None
                }
            } else {
                None
            };
            if let Some((value, then_block, else_block)) = spliced {
                if value {
                    new_stmts.extend(body.blocks[then_block].stmts.clone());
                } else if let Some(eb) = else_block {
                    new_stmts.extend(body.blocks[eb].stmts.clone());
                }
                continue;
            }
            new_stmts.push(s);
        }
        body.blocks[block].stmts = new_stmts;
        true
    }

    /// Bottom-up reduce the subtree rooted at `e` over the kinds the engine
    /// understands (Binary / Unary / Cast / If / Match), applying
    /// [`Self::reduce_local_a`] at each node so a child fold is observable at
    /// its parent. Used by CTFE (`try_call_fold_a`) to evaluate a callee tail
    /// whose children no outer walk has pre-reduced.
    pub fn reduce_in_place_a(&mut self, body: &mut Body, e: ExprId) -> bool {
        let mut changed = match &body.exprs[e].kind {
            ExprKind::Binary { left, right, .. } => {
                let (l, r) = (*left, *right);
                let a = self.reduce_in_place_a(body, l);
                let b = self.reduce_in_place_a(body, r);
                a || b
            }
            ExprKind::Unary { expr: inner, .. } | ExprKind::Cast { expr: inner, .. } => {
                let i = *inner;
                self.reduce_in_place_a(body, i)
            }
            ExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                let (c, t, e2) = (*condition, *then_branch, *else_branch);
                let mut ch = self.reduce_in_place_a(body, c);
                ch |= self.reduce_in_place_block_a(body, t);
                if let Some(eb) = e2 {
                    ch |= self.reduce_in_place_block_a(body, eb);
                }
                ch
            }
            ExprKind::Match {
                expr: scrutinee,
                arms,
            } => {
                let scrutinee = *scrutinee;
                let arm_data: Vec<(Option<ExprId>, ExprId)> =
                    arms.iter().map(|a| (a.guard, a.body)).collect();
                let mut ch = self.reduce_in_place_a(body, scrutinee);
                for (guard, arm_body) in arm_data {
                    if let Some(g) = guard {
                        ch |= self.reduce_in_place_a(body, g);
                    }
                    ch |= self.reduce_in_place_a(body, arm_body);
                }
                ch
            }
            _ => false,
        };
        changed |= self.reduce_local_a(body, e);
        changed
    }

    /// Block-level recursion for [`Self::reduce_in_place_a`].
    fn reduce_in_place_block_a(&mut self, body: &mut Body, block: BlockId) -> bool {
        let stmts = body.blocks[block].stmts.clone();
        let mut changed = false;
        for s in stmts {
            changed |= self.reduce_in_place_stmt_a(body, s);
        }
        changed |= self.reduce_local_block_a(body, block);
        changed
    }

    /// Statement-level recursion for [`Self::reduce_in_place_a`].
    fn reduce_in_place_stmt_a(&mut self, body: &mut Body, s: StmtId) -> bool {
        match &body.stmts[s].kind {
            StmtKind::Expr(e) => {
                let e = *e;
                self.reduce_in_place_a(body, e)
            }
            StmtKind::Let { value, .. } | StmtKind::LetDestructure { value, .. } => {
                let v = *value;
                self.reduce_in_place_a(body, v)
            }
            StmtKind::Return { value } | StmtKind::Break { value, .. } => match *value {
                Some(v) => self.reduce_in_place_a(body, v),
                None => false,
            },
            StmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                let (c, t, e2) = (*condition, *then_block, *else_block);
                let mut ch = self.reduce_in_place_a(body, c);
                ch |= self.reduce_in_place_block_a(body, t);
                if let Some(eb) = e2 {
                    ch |= self.reduce_in_place_block_a(body, eb);
                }
                ch
            }
            StmtKind::Loop { body: b } => {
                let b = *b;
                self.reduce_in_place_block_a(body, b)
            }
            StmtKind::LabeledBlock { block, .. } => {
                let b = *block;
                self.reduce_in_place_block_a(body, b)
            }
            StmtKind::Continue => false,
        }
    }

    /// Project `e` to a lattice, assuming its children are already reduced (the
    /// const-fold visitor walks bottom-up): `try_fold_a` sees folded children
    /// directly, and a non-foldable node falls through to `expr_to_lattice_a`.
    pub fn reduce_to_lattice_a(&self, body: &Body, e: ExprId) -> Lattice {
        match self.try_fold_a(body, e) {
            Lattice::Unevaluated => self.expr_to_lattice_a(body, e),
            other => other,
        }
    }

    /// Reduce the subtree bottom-up in place (so multi-level constant operands
    /// fold), then project to a lattice. The standalone entry point for callers
    /// with an unreduced expression — the `niri` unit tests.
    pub fn reduce_to_lattice_full_a(&mut self, body: &mut Body, e: ExprId) -> Lattice {
        self.reduce_in_place_a(body, e);
        self.reduce_to_lattice_a(body, e)
    }

    /// Collapse an `if` with a constant condition or equal arms.
    fn rewrite_if_expr_a(&mut self, body: &mut Body, e: ExprId) -> bool {
        let (condition, then_branch, else_branch) = match &body.exprs[e].kind {
            ExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => (*condition, *then_branch, *else_branch),
            _ => return false,
        };
        let cond_lat = self.expr_to_lattice_a(body, condition);

        // (1) Constant condition → splice the chosen arm.
        if let Lattice::Const(Value::Bool(b)) = cond_lat {
            body.exprs[e].kind = if b {
                ExprKind::Block(then_branch)
            } else if let Some(eb) = else_branch {
                ExprKind::Block(eb)
            } else {
                ExprKind::Unit
            };
            return true;
        }

        // (2)/(3) require both arms Const.
        let Lattice::Const(t) = self.block_lattice_a(body, then_branch) else {
            return false;
        };
        let Some(eb) = else_branch else {
            return false;
        };
        let Lattice::Const(ev) = self.block_lattice_a(body, eb) else {
            return false;
        };

        // (2) Bool-arms collapse.
        if let (Value::Bool(t_b), Value::Bool(e_b)) = (t, ev)
            && t_b != e_b
        {
            if t_b {
                let cond_kind = body.exprs[condition].kind.clone();
                body.exprs[e].kind = cond_kind;
            } else {
                body.exprs[e].kind = ExprKind::Unary {
                    op: NirUnaryOp::Not,
                    expr: condition,
                };
            }
            return true;
        }

        // (3) Both-arms-equal collapse.
        if t != ev {
            return false;
        }
        if !is_speculatable_a(body, condition) {
            return false;
        }
        body.exprs[e].kind = value_to_arena_kind(t);
        true
    }

    /// Collapse a `match` with a constant scrutinee or a bool-discriminator shape.
    fn rewrite_match_expr_a(&mut self, body: &mut Body, e: ExprId) -> bool {
        let scrutinee = match &body.exprs[e].kind {
            ExprKind::Match { expr, arms } if !arms.is_empty() => *expr,
            _ => return false,
        };
        let arms_data: Vec<(Option<ExprId>, PatId, ExprId, crate::token::Span)> =
            match &body.exprs[e].kind {
                ExprKind::Match { arms, .. } => arms
                    .iter()
                    .map(|a| (a.guard, a.pattern, a.body, a.span))
                    .collect(),
                _ => unreachable!(),
            };

        // Rule 1: const scrutinee → splice the chosen arm.
        if let Lattice::Const(scrut_v) = self.expr_to_lattice_a(body, scrutinee) {
            let mut chosen: Option<usize> = None;
            for (i, (guard, pat, _, _)) in arms_data.iter().enumerate() {
                if guard.is_some() {
                    return false;
                }
                match self.pattern_matches_a(body, &scrut_v, *pat) {
                    PatternMatch::Yes => {
                        chosen = Some(i);
                        break;
                    }
                    PatternMatch::No => {}
                    PatternMatch::Unknown => return false,
                }
            }
            let Some(idx) = chosen else {
                return false;
            };
            let body_e = arms_data[idx].2;
            let span = body.exprs[body_e].span;
            let stmt = body.stmts.push(StmtNode {
                kind: StmtKind::Expr(body_e),
                span,
            });
            let block = body.blocks.push(BlockNode {
                stmts: vec![stmt],
                span,
            });
            body.exprs[e].kind = ExprKind::Block(block);
            return true;
        }

        // Rule 2: `match X { Pat => true, _ => false } → <discriminator>`.
        // The scrutinee is preserved inside the synthesised `Binary`, and the
        // `Match` node `e` keeps its own span — only its `kind` is replaced.
        if let Some(replacement) = try_match_bool_discriminator_a(body, &arms_data) {
            let right = body.exprs.push(ExprNode {
                kind: ExprKind::EnumConstruct {
                    enum_type: replacement.enum_type,
                    case_index: replacement.case_index,
                    case_name: replacement.case_name,
                },
                type_id: replacement.enum_type,
                span: replacement.span,
            });
            body.exprs[e].kind = ExprKind::Binary {
                left: scrutinee,
                op: NirBinaryOp::Eq,
                right,
            };
            return true;
        }

        // Rule 3: non-const speculatable scrutinee, all-arms-equal.
        if !is_speculatable_a(body, scrutinee) {
            return false;
        }
        if arms_data.iter().any(|(g, _, _, _)| g.is_some()) {
            return false;
        }
        let arms_for_exh: Vec<ArmData> = match &body.exprs[e].kind {
            ExprKind::Match { arms, .. } => arms.clone(),
            _ => unreachable!(),
        };
        if !is_provably_exhaustive_a(body, &arms_for_exh) {
            return false;
        }
        let mut common: Option<Value> = None;
        for (_, _, b, _) in &arms_data {
            let Lattice::Const(v) = self.expr_to_lattice_a(body, *b) else {
                return false;
            };
            match common {
                None => common = Some(v),
                Some(c) if c != v => return false,
                Some(_) => {}
            }
        }
        let v = common.expect("at least one arm");
        body.exprs[e].kind = value_to_arena_kind(v);
        true
    }

    /// Fold a pure call whose args are all constant: bind the params, evaluate
    /// the callee's single tail expression, and return `Const(v)` only when it
    /// reduces to a value. `Unevaluated` on any miss (non-call, unknown or
    /// recursive callee, non-const arg, unrecognized body, exhausted budget),
    /// so the original call — and any runtime trap inside it — survives.
    fn try_call_fold_a(&mut self, body: &Body, e: ExprId) -> Lattice {
        let Some(callees) = self.callees else {
            return Lattice::Unevaluated;
        };
        let (func, args): (crate::nir::FunctionRef, Vec<ExprId>) = match &body.exprs[e].kind {
            ExprKind::Call { func, args, .. } => {
                (func.clone(), args.iter().map(|a| a.expr).collect())
            }
            _ => return Lattice::Unevaluated,
        };
        let key: CalleeKey = (func.module_source.clone(), func.full_name());
        let Some(callee_rc) = callees.get(&key) else {
            return Lattice::Unevaluated;
        };
        if self.call_stack.iter().any(|k| k == &key) {
            return Lattice::Unevaluated;
        }
        let Ok(callee) = callee_rc.try_borrow() else {
            return Lattice::Unevaluated;
        };
        let mut bound: Vec<Value> = Vec::with_capacity(args.len());
        for arg in &args {
            match self.expr_to_lattice_a(body, *arg).as_const() {
                Some(v) => bound.push(v),
                None => return Lattice::Unevaluated,
            }
        }
        if bound.len() != callee.params.len() {
            return Lattice::Unevaluated;
        }
        let Some(callee_body) = callee.body.as_ref() else {
            return Lattice::Unevaluated;
        };
        let Some(tail) = single_tail_expression_a(callee_body) else {
            return Lattice::Unevaluated;
        };
        if self.step_budget == 0 {
            return Lattice::Unevaluated;
        }
        self.step_budget -= 1;
        self.call_stack.push(key);
        let saved_env = std::mem::take(&mut self.env);
        for (i, v) in bound.iter().enumerate() {
            #[allow(clippy::cast_possible_truncation)]
            self.env.insert(i as u32, Lattice::Const(*v));
        }
        // Reduce the tail on a scratch copy of the callee's nodes so the shared
        // callee body (held under an immutable `Ref`) is not mutated. Only the
        // node maps are cloned (`nodes_only_clone`) — reduction reads no
        // function-level metadata, so cloning the callee's `locals` would be
        // pure waste.
        let mut scratch = callee_body.nodes_only_clone();
        self.reduce_in_place_a(&mut scratch, tail);
        let result = self.reduce_to_lattice_a(&scratch, tail);
        self.env = saved_env;
        self.call_stack.pop();
        match result {
            c @ Lattice::Const(_) => c,
            Lattice::NonConst | Lattice::Unevaluated => Lattice::Unevaluated,
        }
    }

    /// Look up a `(module_source, name)` global in the installed
    /// [`GlobalEnv`]. Absent keys default to [`Lattice::Unevaluated`]
    /// — the engine simply has no information, same convention as
    /// un-bound locals.
    ///
    /// `IndexMap` lookup needs an owned tuple key, so each call clones
    /// `ModuleSource` (one `String` allocation per variant) and the
    /// global name. If profiling shows this on a hot path, switch the
    /// env to `IndexMap<ModuleSource, IndexMap<String, Lattice>>` or
    /// implement `Borrow`-keyed lookup; it's left flat for now since
    /// `GlobalVarGet` nodes are sparse compared to local reads.
    fn global_lattice(&self, module_source: &ModuleSource, name: &str) -> Lattice {
        let Some(globals) = self.globals else {
            return Lattice::Unevaluated;
        };
        globals
            .get(&(module_source.clone(), name.to_string()))
            .copied()
            .unwrap_or(Lattice::Unevaluated)
    }
}

/// The tail expression of a body whose root block is a single statement —
/// `Return { Some(e) }` or `Expr(e)`. `None` for any other shape, which the
/// caller treats as "do not fold this call".
fn single_tail_expression_a(body: &Body) -> Option<ExprId> {
    let [single] = body.blocks[body.root].stmts.as_slice() else {
        return None;
    };
    match body.stmts[*single].kind {
        StmtKind::Return { value: Some(e) } | StmtKind::Expr(e) => Some(e),
        _ => None,
    }
}

/// `Some(v)` ↦ `Const(v)`, `None` ↦ `NonConst`. Used at the boundary
/// where a numeric-evaluation helper that still returns `Option<Value>`
/// (because its failure modes are runtime traps, not "haven't tried")
/// flows back into the lattice surface.
fn option_to_lattice(opt: Option<Value>) -> Lattice {
    match opt {
        Some(v) => Lattice::Const(v),
        None => Lattice::NonConst,
    }
}

/// Outcome of testing a pattern against a constant scrutinee
/// [`Value`]. The three states mirror the pattern's contribution to
/// SCCP feasibility in [`Interpreter::match_lattice`]:
///
/// - `Yes` — the pattern provably matches; later arms are infeasible
///   edges.
/// - `No` — the pattern provably does not match; this arm is an
///   infeasible edge.
/// - `Unknown` — the engine cannot decide (an unmodelled pattern
///   shape, a guard the engine doesn't analyze, a `ConstantValue`
///   whose inner expression doesn't reduce). The arm stays in play
///   and contributes to the join with all later arms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PatternMatch {
    Yes,
    No,
    Unknown,
}

fn bool_to_match(b: bool) -> PatternMatch {
    if b {
        PatternMatch::Yes
    } else {
        PatternMatch::No
    }
}

/// Join a slice of lattice values via [`Lattice::join`]. Empty input
/// returns [`Lattice::Unevaluated`] (the join's identity).
fn join_all(lats: &[Lattice]) -> Lattice {
    let mut acc = Lattice::Unevaluated;
    for &l in lats {
        acc = acc.join(l);
    }
    acc
}

/// Compare an integer value (raw bits + prim) against a signed i128
/// pattern literal. Returns `true` iff the values are equal under
/// the prim's signedness interpretation.
fn int_value_matches_i128(value: u64, prim: PrimitiveType, pat: i128) -> bool {
    let Some(v) = int_value_as_i128(value, prim) else {
        return false;
    };
    v == pat
}

/// Compare an integer value (raw bits + prim) against an unsigned
/// u128 pattern literal.
fn int_value_matches_u128(value: u64, prim: PrimitiveType, pat: u128) -> bool {
    if is_signed_int(prim) {
        // Signed value cannot represent values outside i64 range
        // anyway; reinterpret as unsigned for comparison.
        let v = value as i64;
        if v < 0 {
            return false;
        }
        u128::from(v as u64) == pat
    } else {
        u128::from(value) == pat
    }
}

/// Convert a (raw bits, prim) integer into an i128, sign- or
/// zero-extending per the prim's signedness. Returns `None` for
/// non-integer prims.
fn int_value_as_i128(value: u64, prim: PrimitiveType) -> Option<i128> {
    if !is_int_prim(prim) {
        return None;
    }
    if is_signed_int(prim) {
        // Stored as sign-extended i64 → widen to i128.
        Some(i128::from(value as i64))
    } else {
        Some(i128::from(value))
    }
}

/// Decide whether a (raw bits, prim) integer falls inside a range
/// pattern. Returns `false` for non-integer prims and for negative
/// signed values against an unsigned-typed range (which by
/// construction starts at zero or higher); otherwise returns the
/// usual half-open / closed range membership test in i128 space.
fn range_matches_int(
    value: u64,
    prim: PrimitiveType,
    start: i128,
    end: i128,
    inclusive: bool,
    is_unsigned_pat: bool,
) -> bool {
    if !is_int_prim(prim) {
        return false;
    }
    let v: i128 = if is_unsigned_pat || !is_signed_int(prim) {
        // Treat the value as unsigned. For a signed prim with negative
        // bits, the unsigned reinterpretation differs — fall back to
        // sign-extended comparison, then ensure it stays nonneg before
        // entering an unsigned range check.
        if is_signed_int(prim) {
            let signed = i128::from(value as i64);
            if signed < 0 {
                // The pattern is unsigned; a negative scrutinee can't
                // be in `[start, end]` when start ≥ 0.
                return false;
            }
            signed
        } else {
            i128::from(value)
        }
    } else {
        i128::from(value as i64)
    };
    if inclusive {
        v >= start && v <= end
    } else {
        v >= start && v < end
    }
}

/// Adjust a block's raw lattice value before feeding it into an
/// arm-feasible-join (the `if` non-constant-condition path).
///
/// `Lattice::Unevaluated` from `block_lattice` means "we couldn't
/// analyze this block's value" — which is fine when the block is the
/// chosen branch of a constant-condition `if` (the other arm is an
/// infeasible edge, so the result really is "we don't know"), but
/// becomes unsound under a non-constant condition: that arm is
/// reachable, the absence of a known value means SCCP-Top
/// (`NonConst`), not infeasibility. Promote here so that a subsequent
/// `Lattice::join` cannot let an `Unevaluated` arm be silently
/// absorbed by a `Const` peer (`join(Unevaluated, Const(v)) → Const(v)`
/// is the infeasible-edge rule, valid only when the Unevaluated edge
/// really is unreachable).
fn arm_lattice_for_feasible_join(lat: Lattice) -> Lattice {
    match lat {
        Lattice::Unevaluated => Lattice::NonConst,
        other => other,
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Value <-> ExprKind bridge
// ──────────────────────────────────────────────────────────────────────────────

fn value_to_arena_kind(v: Value) -> ExprKind {
    match v {
        Value::Int { value, prim } => ExprKind::IntLiteral {
            repr: format_int_repr(value, prim),
            value,
        },
        Value::Float { value, .. } => ExprKind::FloatLiteral {
            repr: format_float_repr(value),
            value,
        },
        Value::Bool(b) => ExprKind::BoolLiteral(b),
        Value::Char(c) => ExprKind::CharLiteral(c),
    }
}

/// Whether the arms cover every scrutinee (a guardless catch-all exists).
fn is_provably_exhaustive_a(body: &Body, arms: &[ArmData]) -> bool {
    arms.iter()
        .any(|a| a.guard.is_none() && pattern_is_catch_all_a(body, a.pattern))
}

fn pattern_is_catch_all_a(body: &Body, pat: PatId) -> bool {
    match &body.pats[pat].kind {
        PatKind::Wildcard | PatKind::Binding { .. } => true,
        PatKind::Or(alts) => alts.iter().any(|p| pattern_is_catch_all_a(body, *p)),
        _ => false,
    }
}

/// Simplify `false && x` / `true || x` and their mirror forms.
fn rewrite_short_circuit_a(body: &mut Body, e: ExprId) -> bool {
    enum Pick {
        Left,
        Right,
    }
    let pick = match &body.exprs[e].kind {
        ExprKind::Binary { left, op, right } => {
            match (&body.exprs[*left].kind, *op, &body.exprs[*right].kind) {
                (ExprKind::BoolLiteral(false), NirBinaryOp::Or, _)
                | (ExprKind::BoolLiteral(true), NirBinaryOp::And, _) => (Pick::Right, *right),
                (_, NirBinaryOp::Or, ExprKind::BoolLiteral(false))
                | (_, NirBinaryOp::And, ExprKind::BoolLiteral(true)) => (Pick::Left, *left),
                _ => return false,
            }
        }
        _ => return false,
    };
    let (_, keep) = pick;
    // Become the kept operand. The other operand is left orphaned.
    let kept = body.exprs[keep].clone();
    body.exprs[e] = kept;
    true
}

/// Recognize `match X { Case => true, _ => false }` as an equality test.
fn try_match_bool_discriminator_a(
    body: &Body,
    arms: &[(Option<ExprId>, PatId, ExprId, crate::token::Span)],
) -> Option<EnumEqReplacement> {
    let [yes_arm, no_arm] = arms else {
        return None;
    };
    if yes_arm.0.is_some() || no_arm.0.is_some() {
        return None;
    }
    if !matches!(body.pats[no_arm.1].kind, PatKind::Wildcard) {
        return None;
    }
    if !matches!(body.exprs[yes_arm.2].kind, ExprKind::BoolLiteral(true)) {
        return None;
    }
    if !matches!(body.exprs[no_arm.2].kind, ExprKind::BoolLiteral(false)) {
        return None;
    }
    let PatKind::Enum {
        enum_type,
        case_name,
        case_index,
    } = &body.pats[yes_arm.1].kind
    else {
        return None;
    };
    Some(EnumEqReplacement {
        enum_type: *enum_type,
        case_index: *case_index,
        case_name: case_name.clone(),
        span: yes_arm.3,
    })
}

/// Whether `e` can be evaluated out of order (side-effect-free, cannot trap).
fn is_speculatable_a(body: &Body, e: ExprId) -> bool {
    match &body.exprs[e].kind {
        ExprKind::IntLiteral { .. }
        | ExprKind::FloatLiteral { .. }
        | ExprKind::BoolLiteral(_)
        | ExprKind::CharLiteral(_)
        | ExprKind::Local { .. }
        | ExprKind::Unit => true,
        ExprKind::Binary { left, op, right } => {
            !matches!(op, NirBinaryOp::Div | NirBinaryOp::Mod)
                && is_speculatable_a(body, *left)
                && is_speculatable_a(body, *right)
        }
        ExprKind::Unary { op, expr: inner } => {
            !matches!(op, NirUnaryOp::Deref) && is_speculatable_a(body, *inner)
        }
        ExprKind::Cast { expr: inner, .. } => is_speculatable_a(body, *inner),
        ExprKind::FieldAccess { expr: inner, .. } => is_speculatable_a(body, *inner),
        _ => false,
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// `match X { Pat => true, _ => false }` discriminator collapse
// ──────────────────────────────────────────────────────────────────────────────

/// The replacement shape produced by `try_match_bool_discriminator`. The
/// scrutinee box is plugged in by the caller once it has taken ownership
/// of the original `Match` expression.
///
/// Only the `EnumEq` shape exists today; `PatKind::Variant` is left
/// intact because synthesising the matching `VariantTest` requires a
/// variant→case-index lookup that the pattern itself doesn't carry
/// (the WIR builder resolves it via the variant decl's case list,
/// which the interpreter doesn't carry today). The fpfmt motivator
/// (`SpecialKind`) is an `enum`, so the Enum-only scope is sufficient
/// for this PR; expanding to `Variant` is a follow-up.
struct EnumEqReplacement {
    enum_type: TypeId,
    case_index: u32,
    case_name: String,
    span: crate::token::Span,
}

impl EnumEqReplacement {}

// ──────────────────────────────────────────────────────────────────────────────
// Pure value evaluation (Bool / Int / Float)
// ──────────────────────────────────────────────────────────────────────────────

/// Evaluate a binary op on two compile-time values.
fn eval_binary(left: Value, op: NirBinaryOp, right: Value) -> Option<Value> {
    match (left, right) {
        (Value::Bool(l), Value::Bool(r)) => eval_bool_binary(l, op, r),
        (Value::Char(l), Value::Char(r)) => eval_char_binary(l, op, r),
        (Value::Float { value: l, prim: lp }, Value::Float { value: r, prim: rp }) if lp == rp => {
            eval_float_binary(l, op, r, lp)
        }
        (Value::Int { value: l, prim: lp }, Value::Int { value: r, prim: rp }) if lp == rp => {
            eval_int_binary(l, op, r, lp)
        }
        _ => None,
    }
}

/// Evaluate a unary op on a compile-time value.
fn eval_unary(op: NirUnaryOp, operand: Value) -> Option<Value> {
    match op {
        NirUnaryOp::Neg => match operand {
            Value::Int { value, prim } => {
                eval_int_neg(value, prim).map(|v| Value::Int { value: v, prim })
            }
            Value::Float { value, prim } => {
                let negated = f64::from_bits(value.to_bits() ^ (1u64 << 63));
                Some(Value::Float {
                    value: negated,
                    prim,
                })
            }
            Value::Bool(_) | Value::Char(_) => None,
        },
        NirUnaryOp::Not => match operand {
            Value::Bool(b) => Some(Value::Bool(!b)),
            _ => None,
        },
        NirUnaryOp::BitNot => match operand {
            Value::Int { value, prim } => Some(Value::Int {
                value: truncate_int(!value, prim),
                prim,
            }),
            _ => None,
        },
        NirUnaryOp::Ref | NirUnaryOp::MutRef | NirUnaryOp::Deref => None,
    }
}

/// Evaluate an `as` cast at compile time.
///
/// Source values are the lattice-resolved [`Value`] of the cast input;
/// `target` is the destination primitive (resolved from the cast node's
/// `type_id`). Returns `None` for unsupported pairs — the caller maps
/// that to [`Lattice::NonConst`] so the runtime cast still happens, no
/// bogus value gets folded in.
///
/// The supported set mirrors what the elaborator permits in source:
///
/// - `Int` source ↦ Int (already supported), Float, Char (only when
///   source is `U8` per [`expr.rs`]'s `u8 as char` carve-out).
/// - `Float` source ↦ Float, Int (saturating, matching Wasm's
///   `*.trunc_sat_*` semantics — Rust's `as` since 1.45 implements the
///   same rounding/saturation rules so we forward to it).
/// - `Bool` source ↦ Int (0/1), Float (0.0/1.0). Bool → Bool is the
///   identity.
/// - `Char` source ↦ Int (codepoint, then truncated). Char → Char is the
///   identity.
///
/// 128-bit (`I128`/`U128`) and SIMD (`V128`) targets are reachable here
/// (they are valid `Primitive` variants) but currently unsupported and
/// fall through to `None`.
fn eval_cast(source: Value, target: PrimitiveType) -> Option<Value> {
    let int_target = is_int_prim(target);
    let float_target = matches!(target, PrimitiveType::F32 | PrimitiveType::F64);
    match source {
        // The source `prim` is irrelevant for int→int because
        // `truncate_int` operates on the already sign- or zero-extended
        // u64 representation set up at construction time.
        Value::Int { value, .. } if int_target => Some(Value::Int {
            value: truncate_int(value, target),
            prim: target,
        }),
        Value::Int { value, prim } if float_target => Some(int_to_float(value, prim, target)),
        // Only `u8 as char` is permitted by the elaborator; every u8 is a
        // valid Unicode scalar, so `char::from(u8)` is total.
        Value::Int {
            value,
            prim: PrimitiveType::U8,
        } if target == PrimitiveType::Char => Some(Value::Char(char::from(value as u8))),

        Value::Float { value, prim } if float_target => Some(float_to_float(value, prim, target)),
        Value::Float { value, prim } if int_target => Some(float_to_int(value, prim, target)),

        Value::Bool(b) if int_target => Some(Value::Int {
            value: u64::from(b),
            prim: target,
        }),
        Value::Bool(b) if float_target => Some(Value::Float {
            value: if b { 1.0 } else { 0.0 },
            prim: target,
        }),
        Value::Bool(b) if target == PrimitiveType::Bool => Some(Value::Bool(b)),

        Value::Char(c) if int_target => Some(Value::Int {
            value: truncate_int(u64::from(c as u32), target),
            prim: target,
        }),
        Value::Char(c) if target == PrimitiveType::Char => Some(Value::Char(c)),

        _ => None,
    }
}

/// True for the eight integer primitives the engine models. 128-bit
/// (`I128`/`U128`) is intentionally excluded — those types lower to
/// stdlib calls in source, not a `Cast` node niri can fold.
fn is_int_prim(p: PrimitiveType) -> bool {
    matches!(
        p,
        PrimitiveType::I8
            | PrimitiveType::I16
            | PrimitiveType::I32
            | PrimitiveType::I64
            | PrimitiveType::U8
            | PrimitiveType::U16
            | PrimitiveType::U32
            | PrimitiveType::U64,
    )
}

/// Convert an integer (held as the sign-extended u64 bit pattern of
/// `prim`) into a float of `target` width. Signed widths are routed
/// through `i64` so the negative range survives; unsigned widths use
/// `u64` directly. F32 results are widened back to f64 so the engine's
/// canonical [`Value::Float`] repr is preserved.
fn int_to_float(value: u64, prim: PrimitiveType, target: PrimitiveType) -> Value {
    let f = if is_signed_int(prim) {
        // `truncate_int` already sign-extended `value` into the i64 range
        // for I8/I16/I32, and an I64 value's u64 bits round-trip through
        // `as i64 as f64`.
        match target {
            PrimitiveType::F32 => f64::from((value as i64) as f32),
            _ => (value as i64) as f64,
        }
    } else {
        match target {
            PrimitiveType::F32 => f64::from(value as f32),
            _ => value as f64,
        }
    };
    Value::Float {
        value: f,
        prim: target,
    }
}

/// Float ↔ float conversion. Widening (f32 → f64) is a no-op on the
/// stored f64 since every f32 is exactly representable; narrowing
/// (f64 → f32) routes through `as f32` to apply the rounding step,
/// then re-widens to f64 for storage. Same-width casts are the identity.
fn float_to_float(value: f64, prim: PrimitiveType, target: PrimitiveType) -> Value {
    let v = match (prim, target) {
        (PrimitiveType::F64, PrimitiveType::F32) => f64::from(value as f32),
        (PrimitiveType::F32 | PrimitiveType::F64, PrimitiveType::F64)
        | (PrimitiveType::F32, PrimitiveType::F32) => value,
        _ => panic!("float_to_float: non-float prim ({prim:?} → {target:?})"),
    };
    Value::Float {
        value: v,
        prim: target,
    }
}

/// Float → integer with Wasm `trunc_sat` semantics: NaN ↦ 0, ±∞ saturate
/// to the target's MIN/MAX, finite values truncate toward zero with
/// saturation. Rust's `as` since 1.45 matches this exactly, so we
/// dispatch through it for the source/target widths that map directly.
///
/// Caller guarantees `target` is one of the i8..u64 primitives (the
/// dispatch in [`eval_cast`] enforces this); panics otherwise to flag
/// a bug rather than fabricate a zero.
fn float_to_int(value: f64, prim: PrimitiveType, target: PrimitiveType) -> Value {
    // For F32 sources the stored f64 is bit-equivalent to the original
    // f32, but the truncation must be performed at f32 precision to
    // match the runtime cast — large magnitudes saturate sooner. Cast
    // back through f32 first when needed; otherwise the f64 path is a
    // no-op widening and the same code computes the answer.
    let raw = match prim {
        PrimitiveType::F32 => trunc_sat_to_int(f64::from(value as f32), target),
        _ => trunc_sat_to_int(value, target),
    };
    Value::Int {
        value: truncate_int(raw, target),
        prim: target,
    }
}

/// Saturating float → int conversion, dispatched by target width.
/// Operates on f64 since every f32 fits exactly; the caller is
/// responsible for narrowing to f32 precision first when the source
/// type was F32.
fn trunc_sat_to_int(value: f64, target: PrimitiveType) -> u64 {
    match target {
        PrimitiveType::I8 => i64::from(value as i8) as u64,
        PrimitiveType::I16 => i64::from(value as i16) as u64,
        PrimitiveType::I32 => i64::from(value as i32) as u64,
        PrimitiveType::I64 => value as i64 as u64,
        PrimitiveType::U8 => u64::from(value as u8),
        PrimitiveType::U16 => u64::from(value as u16),
        PrimitiveType::U32 => u64::from(value as u32),
        PrimitiveType::U64 => value as u64,
        _ => panic!("trunc_sat_to_int: non-integer target {target:?}"),
    }
}

fn eval_bool_binary(l: bool, op: NirBinaryOp, r: bool) -> Option<Value> {
    match op {
        NirBinaryOp::And => Some(Value::Bool(l && r)),
        NirBinaryOp::Or => Some(Value::Bool(l || r)),
        NirBinaryOp::Eq => Some(Value::Bool(l == r)),
        NirBinaryOp::NotEq => Some(Value::Bool(l != r)),
        // bool implements Ord with `false < true`. Spelled with `&&`
        // rather than `<` to satisfy clippy's `bool_comparison` lint
        // without tripping `needless_bitwise_bool`.
        NirBinaryOp::Lt => Some(Value::Bool(!l && r)),
        NirBinaryOp::LtEq => Some(Value::Bool(l <= r)),
        NirBinaryOp::Gt => Some(Value::Bool(l && !r)),
        NirBinaryOp::GtEq => Some(Value::Bool(l >= r)),
        _ => None,
    }
}

/// `char` comparisons. char implements `Eq` and `Ord` (codepoint
/// order); arithmetic / bitwise ops are not defined.
fn eval_char_binary(l: char, op: NirBinaryOp, r: char) -> Option<Value> {
    match op {
        NirBinaryOp::Eq => Some(Value::Bool(l == r)),
        NirBinaryOp::NotEq => Some(Value::Bool(l != r)),
        NirBinaryOp::Lt => Some(Value::Bool(l < r)),
        NirBinaryOp::LtEq => Some(Value::Bool(l <= r)),
        NirBinaryOp::Gt => Some(Value::Bool(l > r)),
        NirBinaryOp::GtEq => Some(Value::Bool(l >= r)),
        _ => None,
    }
}

fn eval_int_binary(lval: u64, op: NirBinaryOp, rval: u64, prim: PrimitiveType) -> Option<Value> {
    match op {
        NirBinaryOp::Add => Some(Value::Int {
            value: truncate_int(lval.wrapping_add(rval), prim),
            prim,
        }),
        NirBinaryOp::Sub => Some(Value::Int {
            value: truncate_int(lval.wrapping_sub(rval), prim),
            prim,
        }),
        NirBinaryOp::Mul => Some(Value::Int {
            value: truncate_int(lval.wrapping_mul(rval), prim),
            prim,
        }),
        NirBinaryOp::Div => eval_int_div(lval, rval, prim).map(|value| Value::Int { value, prim }),
        NirBinaryOp::Mod => eval_int_mod(lval, rval, prim).map(|value| Value::Int { value, prim }),

        NirBinaryOp::Eq
        | NirBinaryOp::NotEq
        | NirBinaryOp::Lt
        | NirBinaryOp::LtEq
        | NirBinaryOp::Gt
        | NirBinaryOp::GtEq => Some(Value::Bool(eval_int_cmp(lval, op, rval, prim))),

        NirBinaryOp::BitAnd => Some(Value::Int {
            value: truncate_int(lval & rval, prim),
            prim,
        }),
        NirBinaryOp::BitOr => Some(Value::Int {
            value: truncate_int(lval | rval, prim),
            prim,
        }),
        NirBinaryOp::BitXor => Some(Value::Int {
            value: truncate_int(lval ^ rval, prim),
            prim,
        }),
        NirBinaryOp::Shl => Some(Value::Int {
            value: eval_int_shl(lval, rval, prim),
            prim,
        }),
        NirBinaryOp::Shr => Some(Value::Int {
            value: eval_int_shr(lval, rval, prim),
            prim,
        }),

        NirBinaryOp::And | NirBinaryOp::Or | NirBinaryOp::RefEq | NirBinaryOp::RefNotEq => None,
    }
}

fn eval_int_cmp(lval: u64, op: NirBinaryOp, rval: u64, prim: PrimitiveType) -> bool {
    if is_signed_int(prim) {
        let l = lval as i64;
        let r = rval as i64;
        match op {
            NirBinaryOp::Eq => l == r,
            NirBinaryOp::NotEq => l != r,
            NirBinaryOp::Lt => l < r,
            NirBinaryOp::LtEq => l <= r,
            NirBinaryOp::Gt => l > r,
            NirBinaryOp::GtEq => l >= r,
            _ => unreachable!(),
        }
    } else {
        match op {
            NirBinaryOp::Eq => lval == rval,
            NirBinaryOp::NotEq => lval != rval,
            NirBinaryOp::Lt => lval < rval,
            NirBinaryOp::LtEq => lval <= rval,
            NirBinaryOp::Gt => lval > rval,
            NirBinaryOp::GtEq => lval >= rval,
            _ => unreachable!(),
        }
    }
}

fn eval_int_shl(lval: u64, rval: u64, prim: PrimitiveType) -> u64 {
    let bits = int_bit_width(prim);
    let shift = (rval as u32) & (bits - 1);
    truncate_int(lval.wrapping_shl(shift), prim)
}

fn eval_int_shr(lval: u64, rval: u64, prim: PrimitiveType) -> u64 {
    let bits = int_bit_width(prim);
    let shift = (rval as u32) & (bits - 1);
    if is_signed_int(prim) {
        let result = (lval as i64).wrapping_shr(shift);
        truncate_int(result as u64, prim)
    } else {
        truncate_int(lval.wrapping_shr(shift), prim)
    }
}

fn eval_int_div(lval: u64, rval: u64, prim: PrimitiveType) -> Option<u64> {
    if rval == 0 {
        return None;
    }
    match prim {
        PrimitiveType::U8 | PrimitiveType::U16 | PrimitiveType::U32 | PrimitiveType::U64 => {
            Some(truncate_int(lval / rval, prim))
        }
        PrimitiveType::I8 => {
            let result = (lval as i8).wrapping_div(rval as i8);
            Some(truncate_int(result as u64, prim))
        }
        PrimitiveType::I16 => {
            let result = (lval as i16).wrapping_div(rval as i16);
            Some(truncate_int(result as u64, prim))
        }
        PrimitiveType::I32 => {
            if lval as i32 == i32::MIN && rval as i32 == -1 {
                return None;
            }
            let result = (lval as i32).wrapping_div(rval as i32);
            Some(truncate_int(result as u64, prim))
        }
        PrimitiveType::I64 => {
            if lval as i64 == i64::MIN && rval as i64 == -1 {
                return None;
            }
            let result = (lval as i64).wrapping_div(rval as i64);
            Some(result as u64)
        }
        _ => None,
    }
}

fn eval_int_mod(lval: u64, rval: u64, prim: PrimitiveType) -> Option<u64> {
    if rval == 0 {
        return None;
    }
    match prim {
        PrimitiveType::U8 | PrimitiveType::U16 | PrimitiveType::U32 | PrimitiveType::U64 => {
            Some(truncate_int(lval % rval, prim))
        }
        PrimitiveType::I8 => {
            let result = (lval as i8).wrapping_rem(rval as i8);
            Some(truncate_int(result as u64, prim))
        }
        PrimitiveType::I16 => {
            let result = (lval as i16).wrapping_rem(rval as i16);
            Some(truncate_int(result as u64, prim))
        }
        PrimitiveType::I32 => {
            if lval as i32 == i32::MIN && rval as i32 == -1 {
                return None;
            }
            let result = (lval as i32).wrapping_rem(rval as i32);
            Some(truncate_int(result as u64, prim))
        }
        PrimitiveType::I64 => {
            if lval as i64 == i64::MIN && rval as i64 == -1 {
                return None;
            }
            let result = (lval as i64).wrapping_rem(rval as i64);
            Some(result as u64)
        }
        _ => None,
    }
}

fn eval_int_neg(value: u64, prim: PrimitiveType) -> Option<u64> {
    match prim {
        PrimitiveType::I8 => {
            let result = (value as i8).wrapping_neg();
            Some(truncate_int(result as u64, prim))
        }
        PrimitiveType::I16 => {
            let result = (value as i16).wrapping_neg();
            Some(truncate_int(result as u64, prim))
        }
        PrimitiveType::I32 => {
            let result = (value as i32).wrapping_neg();
            Some(truncate_int(result as u64, prim))
        }
        PrimitiveType::I64 => {
            let result = (value as i64).wrapping_neg();
            Some(result as u64)
        }
        _ => None,
    }
}

fn eval_float_binary(lval: f64, op: NirBinaryOp, rval: f64, prim: PrimitiveType) -> Option<Value> {
    match prim {
        PrimitiveType::F32 => eval_f32_binary(lval, op, rval),
        PrimitiveType::F64 => eval_f64_binary(lval, op, rval),
        _ => None,
    }
}

fn eval_f64_binary(lval: f64, op: NirBinaryOp, rval: f64) -> Option<Value> {
    match op {
        NirBinaryOp::Add => non_nan_float(lval + rval, PrimitiveType::F64),
        NirBinaryOp::Sub => non_nan_float(lval - rval, PrimitiveType::F64),
        NirBinaryOp::Mul => non_nan_float(lval * rval, PrimitiveType::F64),
        NirBinaryOp::Div => non_nan_float(lval / rval, PrimitiveType::F64),
        _ => eval_float_comparison(lval, op, rval),
    }
}

fn eval_f32_binary(lval: f64, op: NirBinaryOp, rval: f64) -> Option<Value> {
    let l = lval as f32;
    let r = rval as f32;
    match op {
        NirBinaryOp::Add => non_nan_float(f64::from(l + r), PrimitiveType::F32),
        NirBinaryOp::Sub => non_nan_float(f64::from(l - r), PrimitiveType::F32),
        NirBinaryOp::Mul => non_nan_float(f64::from(l * r), PrimitiveType::F32),
        NirBinaryOp::Div => non_nan_float(f64::from(l / r), PrimitiveType::F32),
        NirBinaryOp::Eq => Some(Value::Bool(l == r)),
        NirBinaryOp::NotEq => Some(Value::Bool(l != r)),
        NirBinaryOp::Lt => Some(Value::Bool(l < r)),
        NirBinaryOp::LtEq => Some(Value::Bool(l <= r)),
        NirBinaryOp::Gt => Some(Value::Bool(l > r)),
        NirBinaryOp::GtEq => Some(Value::Bool(l >= r)),
        _ => None,
    }
}

fn eval_float_comparison(lval: f64, op: NirBinaryOp, rval: f64) -> Option<Value> {
    match op {
        NirBinaryOp::Eq => Some(Value::Bool(lval == rval)),
        NirBinaryOp::NotEq => Some(Value::Bool(lval != rval)),
        NirBinaryOp::Lt => Some(Value::Bool(lval < rval)),
        NirBinaryOp::LtEq => Some(Value::Bool(lval <= rval)),
        NirBinaryOp::Gt => Some(Value::Bool(lval > rval)),
        NirBinaryOp::GtEq => Some(Value::Bool(lval >= rval)),
        _ => None,
    }
}

fn non_nan_float(value: f64, prim: PrimitiveType) -> Option<Value> {
    if value.is_nan() {
        return None;
    }
    Some(Value::Float { value, prim })
}

// ──────────────────────────────────────────────────────────────────────────────
// Type queries, truncation, formatting
// ──────────────────────────────────────────────────────────────────────────────

fn is_signed_int(prim: PrimitiveType) -> bool {
    matches!(
        prim,
        PrimitiveType::I8 | PrimitiveType::I16 | PrimitiveType::I32 | PrimitiveType::I64
    )
}

fn int_bit_width(prim: PrimitiveType) -> u32 {
    match prim {
        PrimitiveType::I8 | PrimitiveType::U8 => 8,
        PrimitiveType::I16 | PrimitiveType::U16 => 16,
        PrimitiveType::I32 | PrimitiveType::U32 => 32,
        PrimitiveType::I64 | PrimitiveType::U64 => 64,
        _ => 32,
    }
}

fn is_f32_type(type_id: TypeId, type_table: &TypeTable) -> bool {
    matches!(
        type_table.get(type_id),
        ResolvedType::Primitive(PrimitiveType::F32)
    )
}

/// Resolve any primitive type from a [`TypeId`]. Used by the cast path
/// where the target may be int / float / bool / char (i128/u128/v128
/// are returned but [`eval_cast`] declines to fold them) and by
/// `IntLiteral` lattice resolution after a [`is_int_prim`] filter.
fn prim_of(type_id: TypeId, type_table: &TypeTable) -> Option<PrimitiveType> {
    match type_table.get(type_id) {
        ResolvedType::Primitive(p) => Some(*p),
        _ => None,
    }
}

/// Truncate / sign-extend an integer bit pattern to fit the target prim.
#[must_use]
pub(crate) fn truncate_int(value: u64, prim: PrimitiveType) -> u64 {
    match prim {
        PrimitiveType::U8 => value & 0xFF,
        PrimitiveType::U16 => value & 0xFFFF,
        PrimitiveType::U32 => value & 0xFFFF_FFFF,
        PrimitiveType::U64 => value,
        PrimitiveType::I8 => i64::from(value as i8) as u64,
        PrimitiveType::I16 => i64::from(value as i16) as u64,
        PrimitiveType::I32 => i64::from(value as i32) as u64,
        PrimitiveType::I64 => value,
        _ => value,
    }
}

/// Render an integer bit pattern as decimal text, signed when the prim
/// is signed.
#[must_use]
pub(crate) fn format_int_repr(value: u64, prim: PrimitiveType) -> String {
    if is_signed_int(prim) {
        (value as i64).to_string()
    } else {
        value.to_string()
    }
}

/// Render a `char` as a Wado-friendly literal repr (`'A'`, `'\n'`,
/// `'\u{1F600}'`, …). Used when re-emitting a folded `char` value as a
/// `ExprKind::CharLiteral`.
#[must_use]
pub(crate) fn format_char_repr(c: char) -> String {
    match c {
        '\\' => "'\\\\'".to_string(),
        '\'' => "'\\''".to_string(),
        '\n' => "'\\n'".to_string(),
        '\r' => "'\\r'".to_string(),
        '\t' => "'\\t'".to_string(),
        '\0' => "'\\0'".to_string(),
        c if c.is_ascii_graphic() || c == ' ' => format!("'{c}'"),
        c => format!("'\\u{{{:X}}}'", c as u32),
    }
}

/// Render a float as a Wado-friendly literal repr (`3.25`, `0.0`,
/// `Infinity`, `-Infinity`, …). Trailing `.0` is appended to integral
/// values so the result parses back as a float literal.
#[must_use]
pub(crate) fn format_float_repr(value: f64) -> String {
    if value.is_infinite() {
        return if value.is_sign_positive() {
            "Infinity".to_string()
        } else {
            "-Infinity".to_string()
        };
    }
    let s = value.to_string();
    if s.contains('.') || s.contains('e') || s.contains('E') {
        s
    } else {
        format!("{s}.0")
    }
}
