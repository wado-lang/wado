//! TIR Interpreter (tiri).
//!
//! Compile-time partial evaluator for Wado TIR. The public entry point is
//! [`Interpreter::reduce`], which takes a [`TirExpr`] and returns the most
//! reduced form possible (a literal node when the expression is fully
//! known, the original tree otherwise). Constant folding is the first
//! consumer; future passes (branch pruning, constant propagation,
//! compile-time function evaluation) will reuse the same engine.
//!
//! ```text
//! Interpreter::new(type_table).reduce(&expr) -> TirExpr
//! ```
//!
//! `reduce` is **idempotent** — `reduce(reduce(e))` is structurally equal
//! to `reduce(e)` — and **monotone** — it only moves expressions toward
//! literal form, never the reverse. Literal leaves are preserved as-is so
//! the original lexical repr (e.g. `0xFF`) survives a no-op pass.
//!
//! Today the engine handles:
//!
//! - Integer arithmetic: Add, Sub, Mul, Div, Mod
//! - Integer comparison: Eq, `NotEq`, Lt, `LtEq`, Gt, `GtEq`
//! - Integer bitwise: `BitAnd`, `BitOr`, `BitXor`, Shl, Shr
//! - Integer unary: Neg, `BitNot`
//! - Integer types: i8, i16, i32, i64, u8, u16, u32, u64
//! - Integer cast: truncation/extension between integer types
//! - Float arithmetic: Add, Sub, Mul, Div (skipped when result is NaN)
//! - Float comparison: Eq, `NotEq`, Lt, `LtEq`, Gt, `GtEq`
//! - Float unary: Neg (via sign-bit flip, safe for all values including NaN)
//! - Float types: f32, f64
//! - Boolean logical: And, Or (including identity rules `false || X → X`,
//!   `true && X → X`, `X || false → X`, `X && true → X`)
//! - Boolean equality: Eq, `NotEq`
//! - Boolean unary: Not
//! - Local variables (Stage 1): immutable `let` bindings whose RHS reduces
//!   to a constant flow into the env and are read back as that constant
//!   in their use sites. `let mut` and post-assign locals stay
//!   `NonConst`. The driving visitor populates the env via
//!   [`Interpreter::bind_local`] / [`Interpreter::invalidate_local`].
//! - `if` expressions (Stage 2): a constant condition collapses the node
//!   to the chosen arm; a non-constant condition with both arms reducing
//!   to the same lattice constant (and an effect-free condition) folds
//!   to that constant. The unreachable arm of a constant-condition `if`
//!   is treated as an infeasible SCCP edge — its value never participates
//!   in the join, so a trapping branch (`else { panic(…) }`) does not
//!   contaminate the result.
//! - `if` statements (Stage 2): a constant condition splices the chosen
//!   branch's stmts into the parent block via
//!   [`Interpreter::reduce_local_block`].
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

use crate::hashmap::IndexMap;
use crate::tir::{
    PrimitiveType, ResolvedType, TirBinaryOp, TirBlock, TirExpr, TirExprKind, TirStmt, TirStmtKind,
    TirUnaryOp, TypeId, TypeTable,
};

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
    /// No information yet. Default for un-bound locals and TIR kinds
    /// the engine doesn't currently understand (e.g. `Call`, `Block`).
    /// Stage 3+ may later refine an `Unevaluated` cell to `Const(_)`
    /// once pure-call inlining lands.
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

    /// Render the value as a TIR-compatible literal repr string.
    #[must_use]
    pub fn format_repr(&self) -> String {
        match self {
            Self::Int { value, prim } => format_int_repr(*value, *prim),
            Self::Float { value, .. } => format_float_repr(*value),
            Self::Bool(b) => b.to_string(),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Interpreter
// ──────────────────────────────────────────────────────────────────────────────

/// Partial evaluator over [`TirExpr`].
///
/// Holds the type table needed to resolve operand widths and a
/// per-function `env` mapping local indices to lattice values. Future
/// extensions (step budget for loops, pure-call inlining via the
/// `FlatPackage`) will live on this struct.
pub struct Interpreter<'a> {
    type_table: &'a TypeTable,
    /// Lattice values for `let`-bound locals in the *current function*.
    /// Populated by the driving visitor via [`bind_local`] /
    /// [`invalidate_local`]; cleared via [`enter_function`]. Reads of
    /// `TirExprKind::Local` consult this map during folding.
    ///
    /// Locals not present in the map default to [`Lattice::Unevaluated`].
    ///
    /// [`bind_local`]: Self::bind_local
    /// [`invalidate_local`]: Self::invalidate_local
    /// [`enter_function`]: Self::enter_function
    env: IndexMap<u32, Lattice>,
}

impl<'a> Interpreter<'a> {
    #[must_use]
    pub fn new(type_table: &'a TypeTable) -> Self {
        Self {
            type_table,
            env: IndexMap::default(),
        }
    }

    /// Reset the per-function environment. The driving visitor must call
    /// this before walking each function body; otherwise a previous
    /// function's bindings would leak into the next one (local indices
    /// are unique per function, not project-wide).
    pub fn enter_function(&mut self) {
        self.env.clear();
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
    /// invalidate the prior binding.
    pub fn invalidate_local(&mut self, index: u32) {
        self.env.insert(index, Lattice::NonConst);
    }

    /// Reduce `expr` as far as possible.
    ///
    /// Always returns a (possibly structurally-identical) [`TirExpr`].
    /// Literal leaves are preserved verbatim so their lexical repr
    /// (e.g. `0xFF`) survives a no-op pass.
    pub fn reduce(&mut self, expr: &TirExpr) -> TirExpr {
        let mut owned = expr.clone();
        self.reduce_in_place(&mut owned);
        owned
    }

    /// Recursively reduce `expr` in place over the subtree the engine
    /// currently understands (Binary / Unary / Cast / If). Returns
    /// `true` when anything changed.
    ///
    /// Internal: the only public entry points are [`reduce`] and
    /// [`reduce_local`]. `reduce` clones into `reduce_in_place`; visitor
    /// drivers that already walk every TIR kind via
    /// `tir_visitor::opt_walk_expr` should call `reduce_local` directly.
    ///
    /// [`reduce`]: Self::reduce
    /// [`reduce_local`]: Self::reduce_local
    fn reduce_in_place(&mut self, expr: &mut TirExpr) -> bool {
        // Bottom-up: recurse into children first so the local rewrite
        // step at this node sees fully-reduced operands.
        let mut changed = match &mut expr.kind {
            TirExprKind::Binary { left, right, .. } => {
                let l = self.reduce_in_place(left);
                let r = self.reduce_in_place(right);
                l || r
            }
            TirExprKind::Unary { expr: inner, .. } | TirExprKind::Cast { expr: inner, .. } => {
                self.reduce_in_place(inner)
            }
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                let mut c = self.reduce_in_place(condition);
                c |= self.reduce_in_place_block(then_branch);
                if let Some(eb) = else_branch {
                    c |= self.reduce_in_place_block(eb);
                }
                c
            }
            _ => false,
        };

        changed |= self.reduce_local(expr);
        changed
    }

    /// Recursively reduce every expression inside `block` in place.
    /// Used by [`reduce_in_place`] to walk into `if` arms when the
    /// engine is invoked through the owning [`reduce`] entry point
    /// (the visitor-driven path walks blocks itself).
    fn reduce_in_place_block(&mut self, block: &mut TirBlock) -> bool {
        let mut changed = false;
        for stmt in &mut block.stmts {
            changed |= self.reduce_in_place_stmt(stmt);
        }
        changed |= self.reduce_local_block(block);
        changed
    }

    fn reduce_in_place_stmt(&mut self, stmt: &mut TirStmt) -> bool {
        match &mut stmt.kind {
            TirStmtKind::Expr(e) => self.reduce_in_place(e),
            TirStmtKind::Let { value, .. } | TirStmtKind::LetDestructure { value, .. } => {
                self.reduce_in_place(value)
            }
            TirStmtKind::Return { value } | TirStmtKind::Break { value, .. } => {
                value.as_mut().is_some_and(|v| self.reduce_in_place(v))
            }
            TirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                let mut c = self.reduce_in_place(condition);
                c |= self.reduce_in_place_block(then_block);
                if let Some(eb) = else_block {
                    c |= self.reduce_in_place_block(eb);
                }
                c
            }
            TirStmtKind::Loop { body } => self.reduce_in_place_block(body),
            TirStmtKind::LabeledBlock { block, .. } => self.reduce_in_place_block(block),
            TirStmtKind::IfLet {
                scrutinee,
                then_block,
                else_block,
                ..
            } => {
                let mut c = self.reduce_in_place(scrutinee);
                c |= self.reduce_in_place_block(then_block);
                if let Some(eb) = else_block {
                    c |= self.reduce_in_place_block(eb);
                }
                c
            }
            TirStmtKind::Continue
            | TirStmtKind::TaskReturn { .. }
            | TirStmtKind::VariadicForOf { .. } => false,
        }
    }

    /// Apply the engine's rewrite rules to `expr` only — without recursing
    /// into children. Returns `true` when `expr` was rewritten.
    ///
    /// This is the right entry point when the caller is already driving a
    /// TIR walk (for example via `tir_visitor::opt_walk_expr`) and wants
    /// to slot tiri's local rewrites into each visited node. Today the
    /// rules are constant folding for Binary / Unary / Cast, the
    /// short-circuit identity simplifications for `&&` / `||`, and the
    /// Stage-2 `if`-expression rewrites (constant condition collapses to
    /// the chosen arm; non-constant condition with both arms producing
    /// the same lattice constant collapses to that constant when the
    /// condition is effect-free).
    ///
    /// `Local` nodes themselves are never rewritten in place: their env
    /// values are read transparently when computing the parent
    /// expression's fold (`x + 1` → fold by reading `x` from env, no
    /// in-place mutation of the `Local` node). This keeps assignment
    /// targets (`x = …`, `obj.f = …`, `arr[i] = …`) safely opaque.
    pub fn reduce_local(&mut self, expr: &mut TirExpr) -> bool {
        if let Lattice::Const(v) = self.try_fold(expr) {
            expr.kind = value_to_expr_kind(v);
            return true;
        }
        if rewrite_short_circuit(expr) {
            return true;
        }
        self.rewrite_if_expr(expr)
    }

    /// Apply stmt-level rewrites that may expand or contract the stmt
    /// list of `block`. Today this is solely Stage-2 `if`-statement
    /// folding: an `if true { … } else { … }` stmt with a constant
    /// boolean condition is replaced by the chosen branch's stmts in the
    /// parent block; an `if false { … }` with no else is dropped entirely.
    ///
    /// Returns `true` when the block was rewritten. The caller (driving
    /// visitor) is expected to have walked into each stmt's children
    /// before calling this so the conditions are already folded.
    pub fn reduce_local_block(&mut self, block: &mut TirBlock) -> bool {
        let has_constant_if = block.stmts.iter().any(|s| {
            matches!(
                &s.kind,
                TirStmtKind::If { condition, .. }
                    if matches!(condition.kind, TirExprKind::BoolLiteral(_))
            )
        });
        if !has_constant_if {
            return false;
        }
        let old_stmts = std::mem::take(&mut block.stmts);
        for stmt in old_stmts {
            if let TirStmtKind::If { ref condition, .. } = stmt.kind
                && let TirExprKind::BoolLiteral(value) = condition.kind
            {
                let TirStmtKind::If {
                    then_block,
                    else_block,
                    ..
                } = stmt.kind
                else {
                    unreachable!();
                };
                if value {
                    block.stmts.extend(then_block.stmts);
                } else if let Some(eb) = else_block {
                    block.stmts.extend(eb.stmts);
                }
                continue;
            }
            block.stmts.push(stmt);
        }
        true
    }

    /// Stage-2 rewrite for an `if` expression. See [`reduce_local`] for
    /// the rule set; this helper is the implementation.
    fn rewrite_if_expr(&mut self, expr: &mut TirExpr) -> bool {
        let TirExprKind::If {
            condition,
            then_branch: _,
            else_branch: _,
        } = &expr.kind
        else {
            return false;
        };
        let cond_lat = self.expr_to_lattice(condition);

        // Constant condition → splice the chosen arm. The unreachable arm
        // is dropped without ever being asked for a lattice value, so a
        // trapping `else { panic(…) }` does not contaminate the result —
        // this is the SCCP "infeasible edge" treatment.
        if let Lattice::Const(Value::Bool(b)) = cond_lat {
            let TirExprKind::If {
                then_branch,
                else_branch,
                ..
            } = std::mem::replace(&mut expr.kind, TirExprKind::Unit)
            else {
                unreachable!();
            };
            if b {
                expr.kind = TirExprKind::Block(then_branch);
            } else if let Some(eb) = else_branch {
                expr.kind = TirExprKind::Block(eb);
            }
            // false without else: TirExprKind::Unit is already in place.
            return true;
        }

        // Non-constant condition: consider the both-arms-equal collapse.
        // This is safe only when the condition is effect-free, since
        // dropping the `if` drops its evaluation. See
        // [`is_speculatable`] for what counts as effect-free.
        //
        // Require *both* arms to reduce to the same `Const(v)`. Using
        // `Lattice::join` here would be tempting but unsound: join's
        // `Unevaluated ⊔ Const(v) → Const(v)` rule encodes an SCCP
        // infeasible-edge semantic that does not apply when both edges
        // are feasible (an `Unevaluated` arm here means "reachable but
        // value not known", not "unreachable"). Match `Const(_)` on
        // both sides explicitly so an arm we couldn't analyze never
        // erases the surrounding `if`.
        let TirExprKind::If {
            condition,
            then_branch,
            else_branch,
        } = &expr.kind
        else {
            unreachable!();
        };
        if !is_speculatable(condition) {
            return false;
        }
        let Lattice::Const(t) = self.block_lattice(then_branch) else {
            return false;
        };
        let Some(eb) = else_branch else {
            return false;
        };
        let Lattice::Const(e) = self.block_lattice(eb) else {
            return false;
        };
        if t != e {
            return false;
        }
        expr.kind = value_to_expr_kind(t);
        true
    }

    /// Reduce `expr` to a [`Lattice`] value without mutating the
    /// caller's tree.
    ///
    /// This is the engine's canonical query API. Returns:
    ///
    /// - [`Lattice::Const(v)`] when `expr` evaluates to a known value
    ///   (literal leaf, fully-reduced Binary/Unary/Cast, or a `Local`
    ///   bound to `Const(_)` in env)
    /// - [`Lattice::NonConst`] when `expr` is a `Local` known to be
    ///   non-constant, has a `NonConst` operand, or is a fold that
    ///   meets `Const` operands but evidently fails (e.g. div-by-zero,
    ///   NaN-producing float op, `i32::MIN / -1`)
    /// - [`Lattice::Unevaluated`] when the engine can't yet decide
    ///   (un-bound `Local`, unsupported kind such as `Call` or `Block`)
    pub fn reduce_to_lattice(&mut self, expr: &TirExpr) -> Lattice {
        // First reduce children in place — a Const-Const fold inside a
        // child is observable as a literal at the parent. This may turn
        // a Binary into a literal (Const) or leave it as Binary if the
        // fold failed.
        let mut owned = expr.clone();
        self.reduce_in_place(&mut owned);
        // Compute the lattice of the (possibly partially-reduced)
        // expression. `try_fold` handles Binary / Unary / Cast directly,
        // so a Const/Const op whose runtime would trap reports
        // `NonConst` rather than collapsing to `Unevaluated` because the
        // node is structurally still a Binary. For every other kind
        // `try_fold` returns `Unevaluated`; fall through to the literal
        // / Local-env lookup.
        match self.try_fold(&owned) {
            Lattice::Unevaluated => self.expr_to_lattice(&owned),
            other => other,
        }
    }

    /// Map a (possibly already-reduced) `TirExpr` to a `Lattice`. For
    /// literal leaves this is straightforward; for a `Local` node the
    /// env is consulted; for an `if` node the SCCP-style join over the
    /// arm lattices is taken (with the unreachable arm of a
    /// constant-condition `if` excluded — `feasible_edge`); for a `Block`
    /// only the simple case of a single tail expression is modelled
    /// (anything richer falls through to `Unevaluated`); for everything
    /// else the result is `Unevaluated`.
    fn expr_to_lattice(&self, expr: &TirExpr) -> Lattice {
        match &expr.kind {
            TirExprKind::BoolLiteral(b) => Lattice::Const(Value::Bool(*b)),
            TirExprKind::IntLiteral { value, .. } => {
                let Some(prim) = int_primitive_of(expr.type_id, self.type_table) else {
                    return Lattice::Unevaluated;
                };
                Lattice::Const(Value::Int {
                    value: *value,
                    prim,
                })
            }
            TirExprKind::FloatLiteral { value, .. } => {
                let prim = if is_f32_type(expr.type_id, self.type_table) {
                    PrimitiveType::F32
                } else {
                    PrimitiveType::F64
                };
                Lattice::Const(Value::Float {
                    value: *value,
                    prim,
                })
            }
            TirExprKind::Local { index, .. } => {
                self.env.get(index).copied().unwrap_or(Lattice::Unevaluated)
            }
            TirExprKind::Block(b) => self.block_lattice(b),
            TirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                let cond = self.expr_to_lattice(condition);
                match cond {
                    // Feasible edge: only the chosen arm contributes.
                    // The unchosen arm is unreachable and its value
                    // (whatever it is) does not enter the join.
                    Lattice::Const(Value::Bool(true)) => self.block_lattice(then_branch),
                    Lattice::Const(Value::Bool(false)) => match else_branch {
                        Some(eb) => self.block_lattice(eb),
                        None => Lattice::Unevaluated,
                    },
                    // Non-constant / unevaluated / non-bool condition:
                    // both edges are feasible, so the result is the join
                    // of the arms' values. An arm whose `block_lattice`
                    // came back as `Unevaluated` is *not* an infeasible
                    // edge here — the arm IS reachable, we just couldn't
                    // analyze it. Promote Unevaluated → NonConst before
                    // joining so the absent information correctly pushes
                    // the merged lattice up to Top.
                    _ => {
                        let then_lat =
                            arm_lattice_for_feasible_join(self.block_lattice(then_branch));
                        let else_lat = match else_branch {
                            Some(eb) => arm_lattice_for_feasible_join(self.block_lattice(eb)),
                            // No else arm: the if has type Unit, which
                            // has no representable Const value but the
                            // arm IS reachable.
                            None => Lattice::NonConst,
                        };
                        then_lat.join(else_lat)
                    }
                }
            }
            _ => Lattice::Unevaluated,
        }
    }

    /// Lattice value of a block: for Stage 2 we model only the simplest
    /// useful case — a block whose sole stmt is a tail `Expr(e)`. Such
    /// a block evaluates to whatever `e` evaluates to, so we recurse
    /// through `expr_to_lattice`. Empty blocks evaluate to `()`, which
    /// has no representable [`Value`], so they map to `Unevaluated`
    /// (the join with any other arm carries the other arm's value out,
    /// matching the desired SCCP feasible-edge behavior). Everything
    /// else (intermediate `let` / `Assign` / `Loop` / function calls)
    /// is conservatively `Unevaluated` rather than `NonConst` so that
    /// the surrounding `if` stays foldable when the *other* arm is a
    /// constant — an arm we couldn't evaluate is treated like an
    /// infeasible edge, not a contradicting Const.
    fn block_lattice(&self, block: &TirBlock) -> Lattice {
        match block.stmts.as_slice() {
            [] => Lattice::Unevaluated,
            [single] => match &single.kind {
                TirStmtKind::Expr(e) => self.expr_to_lattice(e),
                _ => Lattice::Unevaluated,
            },
            _ => Lattice::Unevaluated,
        }
    }

    /// Try to fold a Binary / Unary / Cast node by looking up each
    /// operand's lattice value (literal or env-resolved local). The
    /// returned lattice mirrors operand state: any `Unevaluated` /
    /// `NonConst` operand short-circuits the result, and an op-level
    /// failure (div-by-zero, NaN, unsupported pair) is `NonConst`.
    fn try_fold(&self, expr: &TirExpr) -> Lattice {
        match &expr.kind {
            TirExprKind::Binary { left, op, right } => {
                let l = match self.expr_to_lattice(left) {
                    Lattice::Const(v) => v,
                    other => return other,
                };
                let r = match self.expr_to_lattice(right) {
                    Lattice::Const(v) => v,
                    other => return other,
                };
                option_to_lattice(eval_binary(l, *op, r))
            }
            TirExprKind::Unary { op, expr: inner } => {
                let v = match self.expr_to_lattice(inner) {
                    Lattice::Const(v) => v,
                    other => return other,
                };
                option_to_lattice(eval_unary(*op, v))
            }
            TirExprKind::Cast { expr: inner, .. } => {
                let Some(target) = int_primitive_of(expr.type_id, self.type_table) else {
                    return Lattice::Unevaluated;
                };
                // Resolve the cast input to its raw u64 bit pattern.
                // Literal leaves carry the bits directly; for any other
                // node (e.g. a Stage 1 env-resolved `Local`), recover
                // the bits via the lattice — but only when the lattice
                // value is itself an integer. Float / Bool / unresolved
                // cases stay `Unevaluated` so the cast doesn't fabricate
                // a bogus integer payload.
                let raw = match &inner.kind {
                    TirExprKind::IntLiteral { value, .. } => *value,
                    TirExprKind::CharLiteral(c) => *c as u64,
                    _ => match self.expr_to_lattice(inner) {
                        Lattice::Const(Value::Int { value, .. }) => value,
                        Lattice::Const(_) => return Lattice::Unevaluated,
                        other => return other,
                    },
                };
                Lattice::Const(cast_int(raw, target))
            }
            _ => Lattice::Unevaluated,
        }
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

/// Conservative effect-free check for an expression that we may want
/// to drop entirely (specifically, the condition of an `if` whose two
/// arms reduce to the same lattice constant). "Effect-free" here means
/// "evaluating this neither traps, mutates, nor calls a function that
/// might do either". The check is structural and only admits a small
/// allow-list — when in doubt, stay conservative and refuse the rewrite.
///
/// Notes:
///
/// - `Binary { Div | Mod, … }` is excluded because integer
///   division-by-zero traps; `Unary { Deref, … }` is excluded for the
///   same reason (a null/invalid reference would trap).
/// - `FieldAccess` over speculatable receivers is allowed: a non-null
///   GC reference's field load cannot trap once we have the reference.
/// - Calls of any flavor are rejected — even pure callees may return
///   different values across invocations (e.g. `random.next()` is
///   marked pure-by-effect in Wado but is not idempotent in the SCCP
///   sense), and tiri does not yet inline pure calls.
fn is_speculatable(expr: &TirExpr) -> bool {
    match &expr.kind {
        TirExprKind::IntLiteral { .. }
        | TirExprKind::FloatLiteral { .. }
        | TirExprKind::BoolLiteral(_)
        | TirExprKind::CharLiteral(_)
        | TirExprKind::Local { .. }
        | TirExprKind::Capture { .. }
        | TirExprKind::Unit => true,
        TirExprKind::Binary { left, op, right } => {
            !matches!(op, TirBinaryOp::Div | TirBinaryOp::Mod)
                && is_speculatable(left)
                && is_speculatable(right)
        }
        TirExprKind::Unary { op, expr: inner } => {
            !matches!(op, TirUnaryOp::Deref) && is_speculatable(inner)
        }
        TirExprKind::Cast { expr: inner, .. } => is_speculatable(inner),
        TirExprKind::FieldAccess { expr: inner, .. } => is_speculatable(inner),
        _ => false,
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// TirExpr <-> Value bridge
// ──────────────────────────────────────────────────────────────────────────────

fn value_to_expr_kind(v: Value) -> TirExprKind {
    match v {
        Value::Int { value, prim } => TirExprKind::IntLiteral {
            repr: format_int_repr(value, prim),
            value,
        },
        Value::Float { value, .. } => TirExprKind::FloatLiteral {
            repr: format_float_repr(value),
            value,
        },
        Value::Bool(b) => TirExprKind::BoolLiteral(b),
    }
}

/// Identity simplifications for short-circuit operators that *preserve*
/// every subexpression. `false || X → X`, `true && X → X`, and the RHS
/// counterparts (`X || false → X`, `X && true → X`). Returns `true`
/// when `expr` was rewritten.
///
/// The reverse direction (`true || X → true`, `false && X → false`)
/// would drop `X`. Even though Wado's `||`/`&&` short-circuit at runtime
/// — so dropping a side that wouldn't have been evaluated is
/// semantically defensible — this engine stays conservative and leaves
/// those rewrites to a future side-effect-aware pass. Mirrors the
/// previous in-visitor behaviour.
fn rewrite_short_circuit(expr: &mut TirExpr) -> bool {
    enum Pick {
        Left,
        Right,
    }
    let pick = match &expr.kind {
        TirExprKind::Binary { left, op, right } => match (&left.kind, *op, &right.kind) {
            (TirExprKind::BoolLiteral(false), TirBinaryOp::Or, _)
            | (TirExprKind::BoolLiteral(true), TirBinaryOp::And, _) => Pick::Right,
            (_, TirBinaryOp::Or, TirExprKind::BoolLiteral(false))
            | (_, TirBinaryOp::And, TirExprKind::BoolLiteral(true)) => Pick::Left,
            _ => return false,
        },
        _ => return false,
    };
    // Take ownership of the Binary by swapping its `kind` out. The
    // placeholder is local to this function and overwritten before we
    // return, so no caller observes a partially-updated `expr`.
    let TirExprKind::Binary { left, right, .. } =
        std::mem::replace(&mut expr.kind, TirExprKind::Unit)
    else {
        unreachable!("matched Binary above");
    };
    *expr = match pick {
        Pick::Left => *left,
        Pick::Right => *right,
    };
    true
}

// ──────────────────────────────────────────────────────────────────────────────
// Pure value evaluation (Bool / Int / Float)
// ──────────────────────────────────────────────────────────────────────────────

/// Evaluate a binary op on two compile-time values.
fn eval_binary(left: Value, op: TirBinaryOp, right: Value) -> Option<Value> {
    match (left, right) {
        (Value::Bool(l), Value::Bool(r)) => eval_bool_binary(l, op, r),
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
fn eval_unary(op: TirUnaryOp, operand: Value) -> Option<Value> {
    match op {
        TirUnaryOp::Neg => match operand {
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
            Value::Bool(_) => None,
        },
        TirUnaryOp::Not => match operand {
            Value::Bool(b) => Some(Value::Bool(!b)),
            _ => None,
        },
        TirUnaryOp::BitNot => match operand {
            Value::Int { value, prim } => Some(Value::Int {
                value: truncate_int(!value, prim),
                prim,
            }),
            _ => None,
        },
        TirUnaryOp::Ref | TirUnaryOp::MutRef | TirUnaryOp::Deref => None,
    }
}

/// Cast an integer bit pattern to a target integer primitive.
fn cast_int(value: u64, target: PrimitiveType) -> Value {
    Value::Int {
        value: truncate_int(value, target),
        prim: target,
    }
}

fn eval_bool_binary(l: bool, op: TirBinaryOp, r: bool) -> Option<Value> {
    match op {
        TirBinaryOp::And => Some(Value::Bool(l && r)),
        TirBinaryOp::Or => Some(Value::Bool(l || r)),
        TirBinaryOp::Eq => Some(Value::Bool(l == r)),
        TirBinaryOp::NotEq => Some(Value::Bool(l != r)),
        _ => None,
    }
}

fn eval_int_binary(lval: u64, op: TirBinaryOp, rval: u64, prim: PrimitiveType) -> Option<Value> {
    match op {
        TirBinaryOp::Add => Some(Value::Int {
            value: truncate_int(lval.wrapping_add(rval), prim),
            prim,
        }),
        TirBinaryOp::Sub => Some(Value::Int {
            value: truncate_int(lval.wrapping_sub(rval), prim),
            prim,
        }),
        TirBinaryOp::Mul => Some(Value::Int {
            value: truncate_int(lval.wrapping_mul(rval), prim),
            prim,
        }),
        TirBinaryOp::Div => eval_int_div(lval, rval, prim).map(|value| Value::Int { value, prim }),
        TirBinaryOp::Mod => eval_int_mod(lval, rval, prim).map(|value| Value::Int { value, prim }),

        TirBinaryOp::Eq
        | TirBinaryOp::NotEq
        | TirBinaryOp::Lt
        | TirBinaryOp::LtEq
        | TirBinaryOp::Gt
        | TirBinaryOp::GtEq => Some(Value::Bool(eval_int_cmp(lval, op, rval, prim))),

        TirBinaryOp::BitAnd => Some(Value::Int {
            value: truncate_int(lval & rval, prim),
            prim,
        }),
        TirBinaryOp::BitOr => Some(Value::Int {
            value: truncate_int(lval | rval, prim),
            prim,
        }),
        TirBinaryOp::BitXor => Some(Value::Int {
            value: truncate_int(lval ^ rval, prim),
            prim,
        }),
        TirBinaryOp::Shl => Some(Value::Int {
            value: eval_int_shl(lval, rval, prim),
            prim,
        }),
        TirBinaryOp::Shr => Some(Value::Int {
            value: eval_int_shr(lval, rval, prim),
            prim,
        }),

        TirBinaryOp::And | TirBinaryOp::Or | TirBinaryOp::RefEq | TirBinaryOp::RefNotEq => None,
    }
}

fn eval_int_cmp(lval: u64, op: TirBinaryOp, rval: u64, prim: PrimitiveType) -> bool {
    if is_signed_int(prim) {
        let l = lval as i64;
        let r = rval as i64;
        match op {
            TirBinaryOp::Eq => l == r,
            TirBinaryOp::NotEq => l != r,
            TirBinaryOp::Lt => l < r,
            TirBinaryOp::LtEq => l <= r,
            TirBinaryOp::Gt => l > r,
            TirBinaryOp::GtEq => l >= r,
            _ => unreachable!(),
        }
    } else {
        match op {
            TirBinaryOp::Eq => lval == rval,
            TirBinaryOp::NotEq => lval != rval,
            TirBinaryOp::Lt => lval < rval,
            TirBinaryOp::LtEq => lval <= rval,
            TirBinaryOp::Gt => lval > rval,
            TirBinaryOp::GtEq => lval >= rval,
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

fn eval_float_binary(lval: f64, op: TirBinaryOp, rval: f64, prim: PrimitiveType) -> Option<Value> {
    match prim {
        PrimitiveType::F32 => eval_f32_binary(lval, op, rval),
        PrimitiveType::F64 => eval_f64_binary(lval, op, rval),
        _ => None,
    }
}

fn eval_f64_binary(lval: f64, op: TirBinaryOp, rval: f64) -> Option<Value> {
    match op {
        TirBinaryOp::Add => non_nan_float(lval + rval, PrimitiveType::F64),
        TirBinaryOp::Sub => non_nan_float(lval - rval, PrimitiveType::F64),
        TirBinaryOp::Mul => non_nan_float(lval * rval, PrimitiveType::F64),
        TirBinaryOp::Div => non_nan_float(lval / rval, PrimitiveType::F64),
        _ => eval_float_comparison(lval, op, rval),
    }
}

fn eval_f32_binary(lval: f64, op: TirBinaryOp, rval: f64) -> Option<Value> {
    let l = lval as f32;
    let r = rval as f32;
    match op {
        TirBinaryOp::Add => non_nan_float(f64::from(l + r), PrimitiveType::F32),
        TirBinaryOp::Sub => non_nan_float(f64::from(l - r), PrimitiveType::F32),
        TirBinaryOp::Mul => non_nan_float(f64::from(l * r), PrimitiveType::F32),
        TirBinaryOp::Div => non_nan_float(f64::from(l / r), PrimitiveType::F32),
        TirBinaryOp::Eq => Some(Value::Bool(l == r)),
        TirBinaryOp::NotEq => Some(Value::Bool(l != r)),
        TirBinaryOp::Lt => Some(Value::Bool(l < r)),
        TirBinaryOp::LtEq => Some(Value::Bool(l <= r)),
        TirBinaryOp::Gt => Some(Value::Bool(l > r)),
        TirBinaryOp::GtEq => Some(Value::Bool(l >= r)),
        _ => None,
    }
}

fn eval_float_comparison(lval: f64, op: TirBinaryOp, rval: f64) -> Option<Value> {
    match op {
        TirBinaryOp::Eq => Some(Value::Bool(lval == rval)),
        TirBinaryOp::NotEq => Some(Value::Bool(lval != rval)),
        TirBinaryOp::Lt => Some(Value::Bool(lval < rval)),
        TirBinaryOp::LtEq => Some(Value::Bool(lval <= rval)),
        TirBinaryOp::Gt => Some(Value::Bool(lval > rval)),
        TirBinaryOp::GtEq => Some(Value::Bool(lval >= rval)),
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

fn int_primitive_of(type_id: TypeId, type_table: &TypeTable) -> Option<PrimitiveType> {
    match type_table.get(type_id) {
        ResolvedType::Primitive(
            p @ (PrimitiveType::I8
            | PrimitiveType::I16
            | PrimitiveType::I32
            | PrimitiveType::I64
            | PrimitiveType::U8
            | PrimitiveType::U16
            | PrimitiveType::U32
            | PrimitiveType::U64),
        ) => Some(*p),
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
