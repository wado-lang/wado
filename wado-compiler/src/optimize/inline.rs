//! Replace a call to a small, non-recursive function with its body, spliced
//! into a labeled block so the `return` becomes a `break`.
//!
//! A body carries two prices, parting company where it keeps a cold arm.
//! `inline_cost` is what running one copy costs and is what the threshold
//! judges; `inline_size` is what holding one costs, cold paths included.
//! `InlineBudget` spends the second over a cap set as a percentage of the unit
//! as the loop found it, cheapest body first, and reports at debug level what it
//! turned down. `cold_outline` is what keeps the two prices in agreement, so no
//! level sets a cap by default.
//!
//! A callee over budget as written is re-read under the constants its callers
//! pass, taken across every call site so admission stays a property of the
//! callee. Fitting folded is not enough on its own — admitting every marginal
//! fold measured -9% on cbor-twitter — so the fold must also delete a loop or
//! halve the body. Such a callee then receives no inlining itself: growing a
//! body worth more copied than called past the budget destroys it, one-way.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use crate::builtin_facts::SideEffect;
use crate::hashmap::{IndexMap, IndexSet};
use crate::name::inline_block_label;
use crate::nir::{FunctionRef, InlineHint, NirFunction, NirLocal, NirUnaryOp};
use crate::nir_arena::{
    ArenaCallArg, ArenaStructField, ArenaStructPatternField, ArmData, BlockId, BlockNode,
    BlockRole, Body, ExprId, ExprKind, ExprNode, NodeRef, Operand, PatId, PatKind, PatNode, StmtId,
    StmtKind, StmtNode,
};
use crate::nir_package::NirPackage;
use crate::nir_value_graph::{ValueId, ValueKind};
use crate::primitive::PrimitiveType;
use crate::tir::{ResolvedType, TypeId, TypeTable};

use cranelift_entity::EntityRef;

use super::arena_query;
use super::dce::callee_descriptor;
use super::gate::{FunctionGate, GatedPass};
use crate::compiler_trace;
use crate::nir::FuncId;
use crate::nir_value_graph::OpaqueSource;
use crate::niri::is_ctfe_eligible;
use crate::optimize::alias::{CallImmutability, call_verdicts, first_param_types};
use crate::optimize::dce::DescriptorCache;
use crate::optimize::mod_ref::compute_fn_effects;
use crate::token::Span;
use crate::trace::filter;

/// Inline cost weights, in emitted Wasm instructions. The threshold is read in
/// the same unit, so "`-O2` inlines a callee of up to N instructions" is a
/// statement about the output rather than about NIR node shape.
mod weight {
    /// One instruction over its operands: arithmetic, a `struct.get`, an
    /// `array.get`, a `ref.test`, a `struct.new`.
    pub const OP: usize = 1;
    /// A call: the `call` itself plus the ABI edge the caller pays for it. Two
    /// rather than one because a callee built out of calls is a driver, and
    /// splicing it exposes nothing the passes downstream can use.
    pub const CALL: usize = 2;
    /// A branch and the block structure around it. Control flow is what makes a
    /// spliced body expensive in the caller — it splits the caller's regions and
    /// costs it register pressure — so it outweighs a straight-line operation.
    pub const BRANCH: usize = 2;
    /// A loop, for the same reason as [`BRANCH`], one step further: a loop
    /// spliced into a caller is a region of its own.
    pub const LOOP: usize = 3;
}

/// The Wasm value type a primitive occupies. `None` for one with no scalar
/// shape of its own — a `v128`, a reference.
fn wasm_shape(type_table: &TypeTable, id: TypeId) -> Option<PrimitiveType> {
    let ResolvedType::Primitive(prim) = type_table.get(id) else {
        return None;
    };
    match prim {
        PrimitiveType::I8
        | PrimitiveType::U8
        | PrimitiveType::I16
        | PrimitiveType::U16
        | PrimitiveType::I32
        | PrimitiveType::U32
        | PrimitiveType::F16
        | PrimitiveType::Bf16
        | PrimitiveType::Bool
        | PrimitiveType::Char => Some(PrimitiveType::I32),
        PrimitiveType::I64 | PrimitiveType::U64 => Some(PrimitiveType::I64),
        PrimitiveType::F32 => Some(PrimitiveType::F32),
        PrimitiveType::F64 => Some(PrimitiveType::F64),
        PrimitiveType::V128 => None,
    }
}

/// Whether a cast emits an instruction, which is what the weights are counted
/// in. `translate_cast` passes a cast between two types of the same Wasm shape
/// straight through — `char`, `u32` and `bool` are all i32 — so charging one
/// prices a callee above what it emits and holds it back from inlining. Only
/// a shape change, or a narrowing the target has to mask, costs anything.
/// Unknown shapes stay charged: this narrows the estimate, never widens it.
fn cast_emits_instruction(type_table: &TypeTable, from: TypeId, to: TypeId) -> bool {
    let (Some(from_shape), Some(to_shape)) =
        (wasm_shape(type_table, from), wasm_shape(type_table, to))
    else {
        return true;
    };
    let narrows = matches!(
        type_table.get(to),
        ResolvedType::Primitive(
            PrimitiveType::I8 | PrimitiveType::U8 | PrimitiveType::I16 | PrimitiveType::U16
        )
    );
    from_shape != to_shape || narrows
}

/// Whether a cast widens an unsigned 32-bit-shaped value to 64 bits, an
/// `i64.extend_i32_u`. A 64-bit target writes a 32-bit result with its upper
/// half cleared, so Cranelift folds the extension into an arithmetic producer or
/// a load (`extend_to_gpr` in the x64 backend), and anywhere else it is a 32-bit
/// register move.
fn is_zero_extension(type_table: &TypeTable, from: TypeId, to: TypeId) -> bool {
    let unsigned = matches!(
        type_table.get(from),
        ResolvedType::Primitive(
            PrimitiveType::U8
                | PrimitiveType::U16
                | PrimitiveType::U32
                | PrimitiveType::Bool
                | PrimitiveType::Char
        )
    );
    unsigned && wasm_shape(type_table, to) == Some(PrimitiveType::I64)
}

/// True when an expression is a `builtin::cold_path()` marker call.
fn is_cold_path_call(body: &Body, id: ExprId, descriptors: &[FunctionRef]) -> bool {
    matches!(
        &body.exprs[id].kind,
        ExprKind::Call { func_id, .. }
            if callee_descriptor(descriptors, *func_id).is_builtin_named("cold_path")
    )
}

/// How a statement ends the reachable, hot portion of its block, for the inline
/// cost walk in [`CostWalk::block`].
enum BlockCut {
    /// Not a cut — keep accumulating cost.
    None,
    /// A `cold_path()` marker: this statement and everything after it is cold,
    /// so neither contributes (counted as zero).
    Cold,
    /// An unconditional divergence (a `return` / `break` / `continue`, or a call
    /// to a `-> !` function such as `panic`): the statement itself is counted,
    /// but the unreachable tail after it is not.
    Diverges,
}

/// Classify whether a statement cuts off the rest of its block from the inline
/// cost estimate.
fn block_cut(
    body: &Body,
    stmt: StmtId,
    type_table: &TypeTable,
    descriptors: &[FunctionRef],
) -> BlockCut {
    match &body.stmts[stmt].kind {
        StmtKind::Expr(e)
            if e.as_expr()
                .is_some_and(|e| is_cold_path_call(body, e, descriptors)) =>
        {
            BlockCut::Cold
        }
        StmtKind::Return { .. } | StmtKind::Break { .. } | StmtKind::Continue => BlockCut::Diverges,
        StmtKind::Expr(e)
            if e.as_expr()
                .is_some_and(|e| type_table.is_never(body.exprs[e].type_id)) =>
        {
            BlockCut::Diverges
        }
        _ => BlockCut::None,
    }
}

/// Which of a body's two prices a walk is computing. Both are counted in
/// [`weight`]'s unit; they differ over cold code, which runs rarely but is
/// emitted all the same.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Price {
    /// What a caller pays to *run* one copy — the question the inline threshold
    /// asks. A `cold_path()` marker ends its block.
    Hot,
    /// What the module pays to *hold* one copy — the question the growth budget
    /// asks. Cold code counts; only what a divergence makes unreachable does
    /// not, since that is never emitted.
    Size,
}

/// The immutable context of the inline cost walk: how many Wasm instructions a
/// callee emits, in the unit [`weight`] defines and under the [`Price`] asked.
///
/// The `seen` set threaded through it charges each promoted value once. The
/// operand graph is hash-consed, so a sub-value reachable from several operands
/// emits once too, and the set also bounds the walk on a wide DAG.
struct CostWalk<'a> {
    body: &'a Body,
    type_table: &'a TypeTable,
    descriptors: &'a [FunctionRef],
    price: Price,
    /// The constants this walk prices the body under, or `None` to price it as
    /// written. See [`ConstView`].
    consts: Option<&'a ConstView<'a>>,
    /// Per-callee price to charge a call, indexed by `FuncId`; `0` for one this
    /// pass will leave as a call. See [`CostWalk::splicing`].
    spliced: &'a [usize],
}

/// What the caller's constant arguments make of a callee's body: which of its
/// parameters hold a constant throughout, which callees fold away on constant
/// arguments, and which of those spin a loop while doing it.
pub(super) struct ConstView<'a> {
    params: &'a IndexSet<u32>,
    foldable: &'a [bool],
    loopy: &'a [bool],
}

/// Promoted values already charged by one walk.
type SeenValues = IndexSet<ValueId>;

impl<'a> CostWalk<'a> {
    fn new(
        body: &'a Body,
        type_table: &'a TypeTable,
        descriptors: &'a [FunctionRef],
        price: Price,
    ) -> Self {
        Self {
            body,
            type_table,
            descriptors,
            price,
            consts: None,
            spliced: &[],
        }
    }

    /// Price the body under the constants `view` says arrive at every call site.
    fn under(mut self, view: &'a ConstView<'a>) -> Self {
        self.consts = Some(view);
        self
    }

    /// Price a call to a callee this pass will splice at what the splice costs,
    /// rather than at the ABI edge. See [`CostWalk::call_price`]; the table
    /// itself is built in [`inline_functions`].
    fn splicing(mut self, spliced: &'a [usize]) -> Self {
        self.spliced = spliced;
        self
    }

    /// What the whole body costs at this walk's price.
    fn whole_body(&self) -> usize {
        self.block(self.body.root, &mut SeenValues::default())
    }

    /// What a call to `callee` costs this body: the body it splices, when the
    /// table says this pass will splice it, and the ABI edge otherwise.
    ///
    /// [`weight::CALL`] is what a call that *stays* a call costs. The driver it
    /// describes comes in cheap on its own edges, while its caller receives every
    /// one of the bodies behind them. `i64::fmt_decimal` is three such calls, two
    /// of them loops, and splicing it put the whole integer-writing path into
    /// `JsonSerializer::serialize_i64` for -6.9% on json-catalog serialize.
    ///
    /// A site handing the callee *any* constant is charged the edge regardless,
    /// because the constant may unroll the loop the table priced:
    /// `push_be(buf, bits, 8)` in `CborSerializer::serialize_f64` is that shape,
    /// and charging it costs cbor-canada serialize 24%. The precise question —
    /// does the fold this constant licenses delete a loop — is
    /// [`fold_drops_loop`], but it reads a [`ConstView`] over the *callee's*
    /// parameters, not one site's arguments, so `null`, `false` and a radix
    /// argument all switch the lookahead off too.
    fn call_price(&self, callee: FuncId, args: &[ArenaCallArg]) -> usize {
        if args.iter().any(|a| self.arg_is_constant(a.expr)) {
            return weight::CALL;
        }
        let spliced = self.spliced.get(callee.index()).copied().unwrap_or(0);
        spliced.max(weight::CALL)
    }

    /// Whether an argument reaches the callee as a compile-time constant:
    /// written as one, or made one by the caller's constants under
    /// [`CostWalk::under`]. A folded walk is exactly where the literal that
    /// unrolls the callee's loop comes from, so reading only the as-written
    /// shape would charge the loop the caller never receives.
    fn arg_is_constant(&self, op: Operand) -> bool {
        match op {
            Operand::Value(v) if self.body.values.kind(v).is_operand_constant() => true,
            other => self.folds(other),
        }
    }

    /// Whether `op` is a constant once the caller's constant arguments are
    /// substituted, so constant folding decides whatever reads it. Only the
    /// shapes `const_folding` itself folds are admitted.
    fn folds(&self, op: Operand) -> bool {
        let Some(view) = self.consts else {
            return false;
        };
        match op {
            Operand::Value(v) => self.value_folds(view, v),
            Operand::Expr(e) => self.expr_folds(view, e),
        }
    }

    fn value_folds(&self, view: &ConstView<'_>, v: ValueId) -> bool {
        let kind = self.body.values.kind(v);
        if kind.is_operand_constant() {
            return true;
        }
        match kind {
            ValueKind::Binary { lhs, rhs, .. } => {
                self.value_folds(view, *lhs) && self.value_folds(view, *rhs)
            }
            ValueKind::Unary { operand, .. } | ValueKind::Cast { operand, .. } => {
                self.value_folds(view, *operand)
            }
            ValueKind::Opaque(oid) => matches!(
                self.body.values.opaque_source(*oid),
                Some(OpaqueSource::Local(l)) if view.params.contains(&l)
            ),
            ValueKind::Const(..) => true,
            ValueKind::Int(..)
            | ValueKind::Float(..)
            | ValueKind::Bool(_)
            | ValueKind::Char(_)
            | ValueKind::Null
            | ValueKind::Unit
            | ValueKind::Select { .. }
            | ValueKind::LoopPhi { .. }
            | ValueKind::FieldAccess { .. } => false,
        }
    }

    fn expr_folds(&self, view: &ConstView<'_>, id: ExprId) -> bool {
        match &self.body.exprs[id].kind {
            ExprKind::Local { index, .. } => view.params.contains(index),
            ExprKind::PackedArray(_) | ExprKind::EnumConstruct { .. } => true,
            ExprKind::Binary { left, right, .. } => self.folds(*left) && self.folds(*right),
            ExprKind::Unary { expr, .. }
            | ExprKind::Cast { expr, .. }
            | ExprKind::FieldAccess { expr, .. }
            | ExprKind::VariantTag { expr }
            | ExprKind::VariantTest { expr, .. }
            | ExprKind::VariantPayload { expr, .. } => self.folds(*expr),
            ExprKind::TupleLiteral { elements } | ExprKind::ArrayLiteral { elements } => {
                elements.iter().all(|&e| self.folds(e))
            }
            ExprKind::StructLiteral { fields, .. } => fields.iter().all(|f| self.folds(f.value)),
            ExprKind::VariantConstruct { payload, .. } => payload.is_none_or(|p| self.folds(p)),
            // A call the compile-time engine runs on constant arguments leaves
            // a literal behind, so it costs the caller nothing.
            ExprKind::Call { func_id, args, .. } => {
                view.foldable.get(func_id.index()).copied().unwrap_or(false)
                    && args.iter().all(|a| self.folds(a.expr))
            }
            ExprKind::LabeledBlock { .. }
            | ExprKind::If { .. }
            | ExprKind::Match { .. }
            | ExprKind::Switch { .. }
            | ExprKind::Index { .. }
            | ExprKind::Assign { .. }
            | ExprKind::GlobalVarGet { .. }
            | ExprKind::GlobalVarSet { .. }
            | ExprKind::CmRawCall { .. }
            | ExprKind::IndirectCall { .. }
            | ExprKind::ClosureToCanonical { .. }
            | ExprKind::Dead => false,
        }
    }

    /// [`Self::block`] over a copy of `seen`, for weighing one arm of a decided
    /// branch against another without either charging the other's values.
    fn block_cloned(&self, block: BlockId, seen: &SeenValues) -> (usize, SeenValues) {
        let mut s = seen.clone();
        let c = self.block(block, &mut s);
        (c, s)
    }

    /// The cost of the one arm a decided branch keeps. The rest are pruned
    /// before the caller sees them, so only the survivor is charged — which one
    /// that is takes running the condition, so the cheapest stands in.
    fn decided_arms(&self, costs: Vec<(usize, SeenValues)>, seen: &mut SeenValues) -> usize {
        let Some((cost, won)) = costs.into_iter().min_by_key(|(c, _)| *c) else {
            return 0;
        };
        *seen = won;
        cost
    }

    /// Cost of a NIR block, stopping once the rest of it becomes cold or
    /// unreachable. The walk ends at the first statement [`block_cut`] flags: a
    /// `cold_path()` marker drops the marker and, at [`Price::Hot`], everything
    /// after it; a diverging statement (`return` / `break` / `continue` or a
    /// `-> !` call such as `panic`) is itself charged but cuts off its
    /// unreachable tail at either price.
    fn block(&self, block: BlockId, seen: &mut SeenValues) -> usize {
        let mut total = 0;
        for &stmt in &self.body.blocks[block].stmts {
            match block_cut(self.body, stmt, self.type_table, self.descriptors) {
                BlockCut::Cold if self.price == Price::Hot => break,
                BlockCut::Cold => {}
                BlockCut::Diverges => {
                    total += self.stmt(stmt, seen);
                    break;
                }
                BlockCut::None => total += self.stmt(stmt, seen),
            }
        }
        total
    }

    fn stmt(&self, stmt: StmtId, seen: &mut SeenValues) -> usize {
        match &self.body.stmts[stmt].kind {
            StmtKind::Expr(expr) => self.operand(*expr, seen),
            // A `let` is a `local.set` the backend folds into its producer, and
            // one whose value is a bare operand disappears in copy propagation.
            StmtKind::Let { value, .. } | StmtKind::LetDestructure { value, .. } => {
                self.operand(*value, seen)
            }
            // A `break L: <expr>` carries a value just like a `return`; charge it
            // so labeled-block-valued callees are not systematically undercounted.
            StmtKind::Return { value } | StmtKind::Break { value, .. } => {
                value.map_or(0, |v| self.operand(v, seen))
            }
            StmtKind::If {
                condition,
                then_block,
                else_block,
                ..
            } => {
                if self.folds(*condition) {
                    let mut arms = vec![self.block_cloned(*then_block, seen)];
                    arms.push(match else_block {
                        Some(b) => self.block_cloned(*b, seen),
                        None => (0, seen.clone()),
                    });
                    return self.decided_arms(arms, seen);
                }
                weight::BRANCH
                    + self.operand(*condition, seen)
                    + self.block(*then_block, seen)
                    + else_block.map_or(0, |b| self.block(b, seen))
            }
            StmtKind::Loop { body } => weight::LOOP + self.block(*body, seen),
            StmtKind::LabeledBlock { block, .. } => self.block(*block, seen),
            StmtKind::Continue => 0,
        }
    }

    /// Cost reached through an operand slot, which holds either a skeleton
    /// expression or a promoted pure value.
    fn operand(&self, op: Operand, seen: &mut SeenValues) -> usize {
        match op {
            Operand::Expr(e) => self.expr(e, seen),
            Operand::Value(v) => self.value(v, seen),
        }
    }

    /// The type an operand carries, where the body records one.
    fn operand_type(&self, op: Operand) -> Option<TypeId> {
        match op {
            Operand::Expr(e) => Some(self.body.exprs[e].type_id),
            Operand::Value(v) => self.body.values.type_of(v),
        }
    }

    /// What a cast from `from` (unknown where the body records no type) to `to`
    /// costs. A zero-extension is emitted but runs as nothing or a register
    /// move, so only [`Price::Size`] charges it.
    fn cast_price(&self, from: Option<TypeId>, to: TypeId) -> usize {
        let Some(from) = from else {
            return weight::OP;
        };
        let runs = match self.price {
            Price::Hot => !is_zero_extension(self.type_table, from, to),
            Price::Size => true,
        };
        usize::from(runs && cast_emits_instruction(self.type_table, from, to)) * weight::OP
    }

    /// Cost of a promoted pure value, charged once per distinct `ValueId`.
    fn value(&self, v: ValueId, seen: &mut SeenValues) -> usize {
        if !seen.insert(v) {
            return 0;
        }
        match self.body.values.kind(v) {
            // A `T.const` / `local.get` the consuming instruction takes in place.
            ValueKind::Int(..)
            | ValueKind::Float(..)
            | ValueKind::Bool(_)
            | ValueKind::Char(_)
            | ValueKind::Null
            | ValueKind::Unit
            | ValueKind::Opaque(_) => 0,
            // An aggregate constant materialises — as a `global.get` once
            // `const_object_globalization` has placed it, as the allocation
            // itself until then.
            ValueKind::Const(..) => weight::OP,
            ValueKind::Binary { lhs, rhs, .. } => {
                weight::OP + self.value(*lhs, seen) + self.value(*rhs, seen)
            }
            ValueKind::Unary { operand, .. } => weight::OP + self.value(*operand, seen),
            ValueKind::Cast { operand, target } => {
                self.cast_price(self.body.values.type_of(*operand), *target)
                    + self.value(*operand, seen)
            }
            // `select_lowering`'s branchless form: one instruction, three operands.
            ValueKind::Select { cond, then, else_ } => {
                weight::OP
                    + self.value(*cond, seen)
                    + self.value(*then, seen)
                    + self.value(*else_, seen)
            }
            // A loop-carried local: the recurrence is the enclosing loop's cost,
            // and the value itself reads as a local.
            ValueKind::LoopPhi { .. } => 0,
            ValueKind::FieldAccess { receiver, .. } => weight::OP + self.value(*receiver, seen),
        }
    }

    fn expr(&self, id: ExprId, seen: &mut SeenValues) -> usize {
        match &self.body.exprs[id].kind {
            // Operand leaves: a `local.get` / `T.const` / `global.get` the
            // consuming instruction takes in place. Charging them is what made a
            // chain of cheap field reads price as high as a call.
            ExprKind::Local { .. }
            | ExprKind::GlobalVarGet { .. }
            | ExprKind::EnumConstruct { .. }
            | ExprKind::Dead => 0,

            // An allocation of its own, not a leaf the consumer takes in place.
            ExprKind::PackedArray(_) => weight::OP,

            // One instruction over its operands.
            ExprKind::Binary { left, right, .. } => {
                weight::OP + self.operand(*left, seen) + self.operand(*right, seen)
            }
            ExprKind::Unary { expr, .. }
            | ExprKind::FieldAccess { expr, .. }
            | ExprKind::VariantTag { expr }
            | ExprKind::VariantTest { expr, .. }
            | ExprKind::VariantPayload { expr, .. } => weight::OP + self.operand(*expr, seen),
            ExprKind::Cast { expr, target_type } => {
                self.cast_price(self.operand_type(*expr), *target_type) + self.operand(*expr, seen)
            }
            ExprKind::Index { expr, index, .. } => {
                weight::OP + self.operand(*expr, seen) + self.operand(*index, seen)
            }
            ExprKind::GlobalVarSet { value, .. } => weight::OP + self.operand(*value, seen),
            // The place supplies the store's own instruction — a `FieldAccess`
            // target lowers to `struct.set` where a read would be `struct.get` —
            // so the assignment adds only its value.
            ExprKind::Assign { target, value } => {
                self.expr(*target, seen) + self.operand(*value, seen)
            }

            // One allocation instruction, the initialisers' own cost, and a
            // push per initialiser past [`FREE_ARITY`]. An aggregate's arity is
            // its type's, so leaving every leaf free — sound where arity is
            // fixed — priced a whole-struct constructor as one instruction.
            ExprKind::TupleLiteral { elements } | ExprKind::ArrayLiteral { elements } => {
                weight::OP
                    + arity_excess(elements.len())
                    + elements
                        .iter()
                        .map(|e| self.operand(*e, seen))
                        .sum::<usize>()
            }
            ExprKind::StructLiteral { fields, .. } => {
                weight::OP
                    + arity_excess(fields.len())
                    + fields
                        .iter()
                        .map(|f| self.operand(f.value, seen))
                        .sum::<usize>()
            }
            ExprKind::VariantConstruct { payload, .. } => {
                weight::OP + payload.map_or(0, |p| self.operand(p, seen))
            }

            // A call is an ABI edge, not an operation — unless the compile-time
            // engine runs it on constant arguments, leaving a literal.
            ExprKind::Call { func_id, args, .. } => {
                if self.folds(Operand::Expr(id)) {
                    return 0;
                }
                self.call_price(*func_id, args)
                    + args
                        .iter()
                        .map(|a| self.operand(a.expr, seen))
                        .sum::<usize>()
            }
            ExprKind::CmRawCall { args, .. } => {
                weight::CALL + args.iter().map(|a| self.operand(*a, seen)).sum::<usize>()
            }
            ExprKind::IndirectCall { callee, args } => {
                weight::CALL
                    + self.operand(*callee, seen)
                    + args.iter().map(|a| self.operand(*a, seen)).sum::<usize>()
            }
            ExprKind::ClosureToCanonical { functor, .. } => {
                weight::CALL + self.operand(*functor, seen)
            }

            // Control flow. Cold arms contribute nothing: `block` stops at a
            // `cold_path()` marker or a diverging statement within each one.
            ExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                if self.folds(*condition) {
                    let mut arms = vec![self.block_cloned(*then_branch, seen)];
                    arms.push(match else_branch {
                        Some(b) => self.block_cloned(*b, seen),
                        None => (0, seen.clone()),
                    });
                    return self.decided_arms(arms, seen);
                }
                weight::BRANCH
                    + self.operand(*condition, seen)
                    + self.block(*then_branch, seen)
                    + else_branch.map_or(0, |b| self.block(b, seen))
            }
            // One branch for the dispatch, and an arm apiece: each is a block
            // spliced into the caller however cheap its body is.
            ExprKind::Match { expr, arms } => {
                if self.folds(*expr) {
                    let costs = arms
                        .iter()
                        .map(|arm| {
                            let mut s = seen.clone();
                            let c = arm.guard.map_or(0, |g| self.operand(g, &mut s))
                                + self.operand(arm.body, &mut s);
                            (c, s)
                        })
                        .collect();
                    return self.decided_arms(costs, seen);
                }
                weight::BRANCH
                    + arms.len() * weight::OP
                    + self.operand(*expr, seen)
                    + arms
                        .iter()
                        .map(|arm| {
                            arm.guard.map_or(0, |g| self.operand(g, seen))
                                + self.operand(arm.body, seen)
                        })
                        .sum::<usize>()
            }
            ExprKind::Switch {
                scrutinee,
                arms,
                default,
                ..
            } => {
                if self.folds(*scrutinee) {
                    let costs = arms
                        .iter()
                        .chain(std::iter::once(default))
                        .map(|&b| self.block_cloned(b, seen))
                        .collect();
                    return self.decided_arms(costs, seen);
                }
                weight::BRANCH
                    + (arms.len() + 1) * weight::OP
                    + self.operand(*scrutinee, seen)
                    + arms.iter().map(|a| self.block(*a, seen)).sum::<usize>()
                    + self.block(*default, seen)
            }

            // Structural: no instruction of its own.
            ExprKind::LabeledBlock { block, .. } => self.block(*block, seen),
        }
    }
}

/// One [`weight::OP`] per initialiser past [`FREE_ARITY`].
fn arity_excess(len: usize) -> usize {
    len.saturating_sub(FREE_ARITY) * weight::OP
}

/// How many operand leaves a node carries before the model charges for them.
/// Two is what a binary — the widest fixed-arity node — already takes free.
const FREE_ARITY: usize = 2;

/// The inline cost of a function body, in the unit [`weight`] defines. `spliced`
/// prices its calls — see [`CostWalk::splicing`]; empty prices them as ABI edges.
fn inline_cost(
    body: &Body,
    type_table: &TypeTable,
    descriptors: &[FunctionRef],
    spliced: &[usize],
) -> usize {
    CostWalk::new(body, type_table, descriptors, Price::Hot)
        .splicing(spliced)
        .whole_body()
}

/// What one copy of `body` occupies, cold paths included — the quantity the
/// growth budget spends. A `cold_path()` branch is free to run and not free to
/// store, so this parts company with [`inline_cost`] exactly where a callee
/// keeps a rare heavy arm: `TreeBuilder::push_row` in the Gale runtime prices at
/// 27 hot and 166 by size.
fn inline_size(body: &Body, type_table: &TypeTable, descriptors: &[FunctionRef]) -> usize {
    CostWalk::new(body, type_table, descriptors, Price::Size).whole_body()
}

/// What a run of statements occupies, on the same terms as [`inline_size`].
/// `cold_outline` weighs a region against the call that would replace it with
/// this, since a region's cost is its whole subtree — a lone `panic(…)` call
/// whose argument builds a message is a statement to look at and 150 to hold.
pub(super) fn region_size(
    body: &Body,
    type_table: &TypeTable,
    descriptors: &[FunctionRef],
    stmts: &[StmtId],
) -> usize {
    let walk = CostWalk::new(body, type_table, descriptors, Price::Size);
    let mut seen = SeenValues::default();
    stmts.iter().map(|&s| walk.stmt(s, &mut seen)).sum()
}

/// What a call to a helper taking `param_count` arguments costs the caller: the
/// call itself, plus reading each argument out of a local.
pub(super) fn call_site_size(param_count: usize) -> usize {
    weight::CALL + param_count * weight::OP
}

/// What splicing a body of `gross` price actually adds to the caller: the body
/// less the call site it replaces.
///
/// The threshold bounds the caller's *increase*, so this is the quantity it
/// judges. Comparing `gross` against it prices every callee as if calling it
/// were free, and so under-inlines by exactly the arity: a four-parameter callee
/// whose call site costs six is worth six more instructions than a nullary one
/// of the same size. [`super::cold_outline`] makes the same trade in the other
/// direction, weighing a region against the call that would replace it.
fn net_cost(gross: usize, param_count: usize) -> usize {
    gross.saturating_sub(call_site_size(param_count))
}

/// What the caller pays for `body` once constant folding has run on it, given
/// the parameters in `view` arrive constant at every call site: a branch a
/// constant decides keeps one arm, and a pure call over constants becomes a
/// literal.
fn inline_cost_folded(
    body: &Body,
    type_table: &TypeTable,
    descriptors: &[FunctionRef],
    view: &ConstView<'_>,
    spliced: &[usize],
) -> usize {
    CostWalk::new(body, type_table, descriptors, Price::Hot)
        .under(view)
        .splicing(spliced)
        .whole_body()
}

fn collect_inner_labels(callee: &Body, node: NodeRef, labels: &mut IndexSet<String>) {
    match node {
        NodeRef::Stmt(s) => {
            if let StmtKind::LabeledBlock { label, .. } = &callee.stmts[s].kind {
                labels.insert(label.clone());
            }
        }
        NodeRef::Expr(e) => {
            if let ExprKind::LabeledBlock { label, .. } = &callee.exprs[e].kind {
                labels.insert(label.clone());
            }
        }
        NodeRef::Block(_) | NodeRef::Pat(_) => {}
    }
    callee.for_each_child(node, |c| collect_inner_labels(callee, c, labels));
}

/// Mints the labels of one caller's inlined blocks. A block encloses the call's
/// arguments, so its label must differ from every label a break there can name.
#[derive(Default)]
struct InlineLabels {
    taken: Option<IndexSet<String>>,
    serial: u32,
}

impl InlineLabels {
    fn fresh(&mut self, caller: &Body, callee: &str) -> String {
        let taken = self.taken.get_or_insert_with(|| {
            let mut taken = IndexSet::default();
            collect_inner_labels(caller, NodeRef::Block(caller.root), &mut taken);
            taken
        });
        loop {
            let label = inline_block_label(callee, self.serial);
            self.serial += 1;
            if !taken.contains(&label) {
                return label;
            }
        }
    }
}

/// Whether the folds `view` licenses delete a loop — directly, or inside a
/// call they turn into a literal. The model prices a loop at three
/// instructions; what it is worth is however many times it spins, so size
/// alone cannot tell a worthwhile fold from a trivial one.
fn fold_drops_loop(body: &Body, view: &ConstView<'_>, walk: &CostWalk<'_>) -> bool {
    for node in arena_query::reachable_nodes(body) {
        let decided_arms: Vec<BlockId> = match node {
            NodeRef::Expr(e) => match &body.exprs[e].kind {
                ExprKind::Call { func_id, args, .. } => {
                    // Both halves: the engine has to be able to run it away
                    // (`foldable`), and there has to be a loop in what goes.
                    if view.foldable.get(func_id.index()).copied().unwrap_or(false)
                        && view.loopy.get(func_id.index()).copied().unwrap_or(false)
                        && args.iter().all(|a| walk.folds(a.expr))
                    {
                        return true;
                    }
                    continue;
                }
                ExprKind::If {
                    condition,
                    then_branch,
                    else_branch,
                } if walk.folds(*condition) => {
                    let mut v = vec![*then_branch];
                    v.extend(else_branch);
                    v
                }
                ExprKind::Switch {
                    scrutinee,
                    arms,
                    default,
                    ..
                } if walk.folds(*scrutinee) => {
                    let mut v = arms.clone();
                    v.push(*default);
                    v
                }
                _ => continue,
            },
            NodeRef::Stmt(st) => match &body.stmts[st].kind {
                StmtKind::If {
                    condition,
                    then_block,
                    else_block,
                    ..
                } if walk.folds(*condition) => {
                    let mut v = vec![*then_block];
                    v.extend(else_block);
                    v
                }
                _ => continue,
            },
            NodeRef::Block(_) | NodeRef::Pat(_) => continue,
        };
        // Every arm but the survivor is deleted, so a loop in any of them is a
        // loop the caller may stop running. Which one survives takes evaluating
        // the condition; one loop among them is evidence enough.
        if decided_arms
            .iter()
            .any(|&b| arena_query::block_contains_loop(body, b))
        {
            return true;
        }
    }
    false
}

/// Whether running `body` reaches a safepoint: a call, or an allocation, where
/// the collector may run. A copying collector moves objects there, so every
/// reference live across one is spilled and reloaded, and a loop holds more of
/// them than the callee does. `safepoint_calls` says which calls are one. The
/// literal a `return` hands back is not: it is the body's last act, so what is
/// live across it is what was live across the call it replaces, and a caller
/// that takes it apart deletes it. Code past a `cold_path()` marker is no part
/// of the run.
fn has_safepoint(
    body: &Body,
    type_table: &TypeTable,
    descriptors: &[FunctionRef],
    safepoint_calls: &[bool],
) -> bool {
    struct Walk<'a> {
        body: &'a Body,
        type_table: &'a TypeTable,
        descriptors: &'a [FunctionRef],
        safepoint_calls: &'a [bool],
    }
    impl Walk<'_> {
        fn block(&self, block: BlockId) -> bool {
            for &stmt in &self.body.blocks[block].stmts {
                match block_cut(self.body, stmt, self.type_table, self.descriptors) {
                    BlockCut::Cold => return false,
                    BlockCut::Diverges => return self.node(NodeRef::Stmt(stmt)),
                    BlockCut::None if self.node(NodeRef::Stmt(stmt)) => return true,
                    BlockCut::None => {}
                }
            }
            false
        }

        fn node(&self, node: NodeRef) -> bool {
            match node {
                NodeRef::Block(b) => return self.block(b),
                NodeRef::Expr(e) if self.is_safepoint(e) => return true,
                NodeRef::Stmt(s) => {
                    if let StmtKind::Return {
                        value: Some(Operand::Expr(e)),
                    } = self.body.stmts[s].kind
                        && allocates(&self.body.exprs[e].kind)
                    {
                        return self.children(NodeRef::Expr(e));
                    }
                }
                NodeRef::Expr(_) | NodeRef::Pat(_) => {}
            }
            self.children(node)
        }

        fn children(&self, node: NodeRef) -> bool {
            let mut found = false;
            self.body
                .for_each_child(node, |c| found = found || self.node(c));
            found
        }

        fn is_safepoint(&self, e: ExprId) -> bool {
            match &self.body.exprs[e].kind {
                ExprKind::Call { func_id, .. } => self.safepoint_calls[func_id.index()],
                ExprKind::IndirectCall { .. } | ExprKind::CmRawCall { .. } => true,
                kind => allocates(kind),
            }
        }
    }
    fn allocates(kind: &ExprKind) -> bool {
        matches!(
            kind,
            ExprKind::StructLiteral { .. }
                | ExprKind::TupleLiteral { .. }
                | ExprKind::ArrayLiteral { .. }
                | ExprKind::PackedArray(_)
                | ExprKind::ClosureToCanonical { .. }
                | ExprKind::VariantConstruct {
                    payload: Some(_),
                    ..
                }
        )
    }
    Walk {
        body,
        type_table,
        descriptors,
        safepoint_calls,
    }
    .block(body.root)
}

/// Whether a call to each function, by `FuncId` index, is a safepoint: any call
/// to a function with a body, a builtin declaring `#[side_effect(opaque)]`
/// (which leaves for the host), and a builtin whose result is freshly
/// allocated. The other builtins are Wasm instructions.
fn safepoint_calls(project: &NirPackage, descriptors: &[FunctionRef]) -> Vec<bool> {
    descriptors
        .iter()
        .map(|callee| {
            project
                .builtin_declarations
                .get(callee)
                .is_none_or(|declaration| {
                    matches!(declaration.facts.side_effect, SideEffect::Opaque)
                        || declaration.allocates()
                })
        })
        .collect()
}

/// What `inline` reads off one body each round, gathered in one walk; empty for
/// a bodyless function.
#[derive(Default)]
struct BodyScan {
    /// Locals the body writes, counting a write anywhere in the place (`p.x =
    /// f()` writes `p`) and a mutable hand-out, which is a write the body does
    /// not show.
    written: IndexSet<u32>,
    /// Every `let`, as the local and the value bound to it.
    lets: Vec<(u32, Operand)>,
    /// Every call, with its by-value arguments keyed by position: an
    /// [`ArgSite`] before the callee's own writes rule any out.
    calls: Vec<ArgSite>,
    /// Whether the body holds a `loop`.
    loopy: bool,
}

fn scan_body(func: &NirFunction) -> BodyScan {
    // A site's arguments are keyed by position and the callee's writes by
    // local index.
    assert!(
        func.params
            .iter()
            .enumerate()
            .all(|(pos, p)| p.local_index == pos as u32),
        "a parameter's local index is its position"
    );
    let mut scan = BodyScan::default();
    let Some(body) = &func.body else {
        return scan;
    };
    body.for_each_reachable_node(|node| match node {
        NodeRef::Stmt(st) => match &body.stmts[st].kind {
            StmtKind::Let {
                local_index, value, ..
            } => scan.lets.push((*local_index, *value)),
            StmtKind::Loop { .. } => scan.loopy = true,
            _ => {}
        },
        NodeRef::Expr(e) => match &body.exprs[e].kind {
            ExprKind::Assign { target, .. } => {
                scan.written.extend(place_root_local(body, *target));
            }
            ExprKind::Unary {
                op: NirUnaryOp::MutRef,
                expr: inner,
            } => {
                scan.written
                    .extend(inner.as_expr().and_then(|x| place_root_local(body, x)));
            }
            // A `&mut` argument is the callee's write. A `&mut` parameter passed
            // on wears no `MutRef` of its own — it is already a reference, so it
            // reaches the next call as a bare place and only `is_mut` says so.
            // `args[0]` carries a method's `&mut self` the same way.
            ExprKind::Call { func_id, args, .. } => {
                for arg in args.iter().filter(|a| a.is_mut) {
                    scan.written
                        .extend(arg.expr.as_expr().and_then(|x| place_root_local(body, x)));
                }
                let by_value = args
                    .iter()
                    .enumerate()
                    .filter(|(_, a)| !a.is_mut)
                    .map(|(pos, a)| (pos as u32, a.expr))
                    .collect();
                scan.calls.push((*func_id, by_value));
            }
            _ => {}
        },
        NodeRef::Block(_) | NodeRef::Pat(_) => {}
    });
    scan
}

/// Locals a body binds once, to a constant, and never writes — the shape a
/// literal argument wears by the time it reaches a call (`let name = "id";
/// f(&name)`), which is otherwise indistinguishable from a runtime value.
fn constant_locals(body: &Body, scan: &BodyScan) -> IndexSet<u32> {
    let mut bound: IndexMap<u32, bool> = IndexMap::default();
    for &(local_index, value) in &scan.lets {
        let is_const = is_constant_arg(body, value, &IndexSet::default());
        match bound.get_mut(&local_index) {
            // A second binding of the same slot: keep it only if both are
            // constant, since either may reach the call.
            Some(prev) => *prev &= is_const,
            None => {
                bound.insert(local_index, is_const);
            }
        }
    }
    let written = &scan.written;
    bound
        .into_iter()
        .filter(|&(idx, is_const)| is_const && !written.contains(&idx))
        .map(|(idx, _)| idx)
        .collect()
}

/// The local a place expression is rooted at, through field, index and deref
/// steps: `p.inner[i]` is rooted at `p`.
fn place_root_local(body: &Body, expr: ExprId) -> Option<u32> {
    match &body.exprs[expr].kind {
        ExprKind::Local { index, .. } => Some(*index),
        ExprKind::FieldAccess { expr: inner, .. }
        | ExprKind::VariantPayload { expr: inner, .. }
        | ExprKind::Unary { expr: inner, .. } => {
            inner.as_expr().and_then(|x| place_root_local(body, x))
        }
        ExprKind::Index { expr: inner, .. } => {
            inner.as_expr().and_then(|x| place_root_local(body, x))
        }
        _ => None,
    }
}

/// Whether an argument reaches the callee as a compile-time constant — the
/// shapes `const_folding` reads as one, plus the `&` a borrowed literal wears
/// and the local it may be bound to first.
fn is_constant_arg(body: &Body, op: Operand, const_locals: &IndexSet<u32>) -> bool {
    match op {
        // `Const` covers the aggregate a string / list literal becomes once
        // `promote_pure_values_early` freezes it.
        Operand::Value(v) => {
            let kind = body.values.kind(v);
            kind.is_operand_constant() || matches!(kind, ValueKind::Const(..))
        }
        Operand::Expr(e) => match &body.exprs[e].kind {
            ExprKind::PackedArray(_) | ExprKind::EnumConstruct { .. } => true,
            ExprKind::Unary { expr, .. } | ExprKind::Cast { expr, .. } => {
                is_constant_arg(body, *expr, const_locals)
            }
            ExprKind::TupleLiteral { elements } | ExprKind::ArrayLiteral { elements } => elements
                .iter()
                .all(|&e| is_constant_arg(body, e, const_locals)),
            ExprKind::StructLiteral { fields, .. } => fields
                .iter()
                .all(|f| is_constant_arg(body, f.value, const_locals)),
            ExprKind::VariantConstruct { payload, .. } => {
                payload.is_none_or(|pl| is_constant_arg(body, pl, const_locals))
            }
            ExprKind::Local { index, .. } => const_locals.contains(index),
            ExprKind::Dead
            | ExprKind::Binary { .. }
            | ExprKind::FieldAccess { .. }
            | ExprKind::VariantTag { .. }
            | ExprKind::VariantTest { .. }
            | ExprKind::VariantPayload { .. }
            | ExprKind::Index { .. }
            | ExprKind::Assign { .. }
            | ExprKind::GlobalVarGet { .. }
            | ExprKind::GlobalVarSet { .. }
            | ExprKind::Call { .. }
            | ExprKind::CmRawCall { .. }
            | ExprKind::IndirectCall { .. }
            | ExprKind::ClosureToCanonical { .. }
            | ExprKind::LabeledBlock { .. }
            | ExprKind::If { .. }
            | ExprKind::Match { .. }
            | ExprKind::Switch { .. } => false,
        },
    }
}

/// One call in a body: the callee, and the arguments that may carry a constant
/// into it — by value, to a parameter the callee never writes, keyed by
/// position. A written parameter is out because the readers here are
/// flow-insensitive: `fn f(mut n: i32) { n = g(); .. }` holds no argument.
type ArgSite = (FuncId, Vec<(u32, Operand)>);

/// The [`ArgSite`]s of every function, indexed by store position; empty for a
/// bodyless one.
fn argument_sites(scans: &[BodyScan]) -> Vec<Vec<ArgSite>> {
    scans
        .iter()
        .map(|scan| {
            scan.calls
                .iter()
                .map(|(callee, args)| {
                    let callee_written = &scans[callee.index()].written;
                    let args = args
                        .iter()
                        .filter(|(pos, _)| !callee_written.contains(pos))
                        .copied()
                        .collect();
                    (*callee, args)
                })
                .collect()
        })
        .collect()
}

/// For each callee, the parameters *every* call site in the program fills with
/// a compile-time constant and the callee's own body never writes.
///
/// Whole-program rather than per-site: admission stays a property of the
/// callee, so a body taken on its folded cost is never spliced at a site that
/// would not fold it. A callee nothing calls is absent.
fn constant_params(
    project: &NirPackage,
    scans: &[BodyScan],
    sites: &[Vec<ArgSite>],
) -> IndexMap<FuncId, IndexSet<u32>> {
    let mut out: IndexMap<FuncId, IndexSet<u32>> = IndexMap::default();
    for (i, func_rc) in project.functions.iter().enumerate() {
        let func = func_rc.borrow();
        let Some(body) = &func.body else {
            continue;
        };
        let const_locals = constant_locals(body, &scans[i]);
        for (callee, args) in &sites[i] {
            let here: IndexSet<u32> = args
                .iter()
                .filter(|&&(_, op)| is_constant_arg(body, op, &const_locals))
                .map(|&(pos, _)| pos)
                .collect();
            match out.get_mut(callee) {
                Some(prev) => prev.retain(|q| here.contains(q)),
                None => {
                    out.insert(*callee, here);
                }
            }
        }
    }
    out.retain(|_, params| !params.is_empty());
    out
}

/// Per function, the parameters it never writes that some call site may still
/// hand a constant ([`Caller::may_turn_constant`]). A hold bets on a constant
/// arriving, and only these can.
///
/// Transitive: a caller's own parameter carries a constant on only once one
/// may reach it, so the sets grow from the literal arguments outward, and a
/// caller's sites are weighed again each time its set grows.
fn hopeful_params(
    project: &NirPackage,
    scans: &[BodyScan],
    sites: &[Vec<ArgSite>],
) -> Vec<IndexSet<u32>> {
    let n = project.functions.len();
    let funcs: Vec<_> = project.functions.iter().map(|f| f.borrow()).collect();
    let callers: Vec<Option<Caller<'_>>> = funcs
        .iter()
        .zip(scans)
        .zip(sites)
        .map(|((func, scan), sites)| {
            func.body.as_ref().map(|body| Caller {
                body,
                written: &scan.written,
                bindings: single_bindings(scan),
                sites,
            })
        })
        .collect();
    let mut hopeful: Vec<IndexSet<u32>> = vec![IndexSet::default(); n];
    let mut queued = vec![true; n];
    let mut worklist: Vec<usize> = (0..n).collect();
    while let Some(c) = worklist.pop() {
        queued[c] = false;
        let Some(caller) = &callers[c] else {
            continue;
        };
        let own = hopeful[c].clone();
        let memo = RefCell::default();
        for (callee, args) in caller.sites {
            let callee = callee.index();
            let mut grew = false;
            for &(pos, op) in args {
                if !hopeful[callee].contains(&pos) && caller.may_turn_constant(op, &own, &memo, 0) {
                    hopeful[callee].insert(pos);
                    grew = true;
                }
            }
            if grew && !queued[callee] {
                queued[callee] = true;
                worklist.push(callee);
            }
        }
    }
    hopeful
}

/// What [`Caller::may_turn_constant`] reads about the body holding a call site.
struct Caller<'a> {
    body: &'a Body,
    written: &'a IndexSet<u32>,
    /// The one value each unwritten, once-bound local holds.
    bindings: IndexMap<u32, Operand>,
    sites: &'a [ArgSite],
}

/// How deep [`Caller::may_turn_constant`] follows an operand before it answers
/// yes, so a long chain of lets cannot take the recursion as deep as itself.
const MAY_TURN_CONSTANT_DEPTH: u32 = 16;

impl Caller<'_> {
    /// Whether a later round may still fold `op` to a constant, given the
    /// parameters of this body a constant may reach. Optimistic: only a value
    /// that no splice or fold can pin — another parameter, a loop-carried
    /// value, an indirect call, a write — answers no.
    ///
    /// `memo` holds each bound local's answer under this `hopeful`: a local
    /// read twice shares its binding, so the bindings form a DAG that an
    /// unmemoized walk covers exponentially.
    fn may_turn_constant(
        &self,
        op: Operand,
        hopeful: &IndexSet<u32>,
        memo: &RefCell<IndexMap<u32, bool>>,
        depth: u32,
    ) -> bool {
        if depth == MAY_TURN_CONSTANT_DEPTH {
            return true;
        }
        let next = |op: Operand| self.may_turn_constant(op, hopeful, memo, depth + 1);
        let e = match op {
            Operand::Value(v) => {
                return !matches!(self.body.values.kind(v), ValueKind::LoopPhi { .. });
            }
            Operand::Expr(e) => e,
        };
        match &self.body.exprs[e].kind {
            ExprKind::Local { index, .. } => {
                if self.written.contains(index) {
                    return false;
                }
                if hopeful.contains(index) {
                    return true;
                }
                let Some(&value) = self.bindings.get(index) else {
                    return false;
                };
                if let Some(&known) = memo.borrow().get(index) {
                    return known;
                }
                let answer = next(value);
                memo.borrow_mut().insert(*index, answer);
                answer
            }
            ExprKind::PackedArray(_)
            | ExprKind::EnumConstruct { .. }
            | ExprKind::GlobalVarGet { .. }
            | ExprKind::LabeledBlock { .. }
            | ExprKind::If { .. }
            | ExprKind::Match { .. }
            | ExprKind::Switch { .. } => true,
            ExprKind::Unary { expr, .. }
            | ExprKind::Cast { expr, .. }
            | ExprKind::FieldAccess { expr, .. }
            | ExprKind::VariantTag { expr, .. }
            | ExprKind::VariantTest { expr, .. }
            | ExprKind::VariantPayload { expr, .. } => next(*expr),
            ExprKind::Binary { left, right, .. } => next(*left) && next(*right),
            ExprKind::Index { expr, index } => next(*expr) && next(*index),
            ExprKind::TupleLiteral { elements } | ExprKind::ArrayLiteral { elements } => {
                elements.iter().all(|&el| next(el))
            }
            ExprKind::StructLiteral { fields, .. } => fields.iter().all(|f| next(f.value)),
            ExprKind::VariantConstruct { payload, .. } => payload.is_none_or(next),
            ExprKind::Call { args, .. } => args.iter().all(|a| !a.is_mut && next(a.expr)),
            ExprKind::Dead
            | ExprKind::Assign { .. }
            | ExprKind::GlobalVarSet { .. }
            | ExprKind::CmRawCall { .. }
            | ExprKind::IndirectCall { .. }
            | ExprKind::ClosureToCanonical { .. } => false,
        }
    }
}

/// The value each local holds where the body binds it once and never writes
/// it again.
fn single_bindings(scan: &BodyScan) -> IndexMap<u32, Operand> {
    let mut bound: IndexMap<u32, Option<Operand>> = IndexMap::default();
    for &(local_index, value) in &scan.lets {
        bound
            .entry(local_index)
            .and_modify(|v| *v = None)
            .or_insert(Some(value));
    }
    bound
        .into_iter()
        .filter(|(idx, _)| !scan.written.contains(idx))
        .filter_map(|(idx, v)| v.map(|v| (idx, v)))
        .collect()
}

/// How much more a callee may cost where its splice pays back than anywhere
/// else: at a site inside a loop, whose call edge is paid on every iteration,
/// and at the callee's only site, where the body moves rather than being
/// copied. Only a callee reaching no safepoint qualifies, so the splice brings
/// its body alone and none of the spills a safepoint costs the loop. At `-O2`
/// this admits up to 64, which reaches the stdlib's UTF-8 codec helpers
/// (`decode_char_at` 50, `encode_char` 61) and the zlib match helpers that once
/// carried an inline hint each.
const PAYBACK_FACTOR: usize = 4;

/// Which call sites splice a callee.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Reach {
    #[default]
    Nowhere,
    /// Only a site inside a loop, where it is the callee's only site: a
    /// callee under [`PAYBACK_FACTOR`] with more than one site.
    Loops,
    /// Only the one site the round began with: a callee under
    /// [`PAYBACK_FACTOR`] with one site. A splice of its caller copies that
    /// site, and the copy is a second one.
    Sole,
    Everywhere,
}

/// Where a call site sits, as far as the price it may pay goes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Site {
    Plain,
    /// Inside a loop of the caller.
    Loop,
    /// After a `cold_path()` marker: only `#[inline(always)]` splices here,
    /// since anything else only bloats the hot caller.
    Cold,
}

impl Site {
    fn entering_loop(self) -> Self {
        match self {
            Site::Cold => Site::Cold,
            Site::Plain | Site::Loop => Site::Loop,
        }
    }
}

/// What the engine decides about one callee.
#[derive(Clone, Copy, Default)]
struct Verdict {
    /// The hot price the verdict was reached on, or `0` for a callee turned
    /// down before it was priced at all.
    hot: usize,
    /// Which of its call sites splice it.
    reach: Reach,
    /// Keep it as a template: splice nothing *into* it.
    ///
    /// A body whose parameters, were they constant, delete a loop from it is
    /// worth more as something callers copy than as something callers call, and
    /// growing it past the budget is one-way. This is what an
    /// `#[inline(never)]` on each leaf otherwise applies by hand.
    hold: bool,
}

/// Whether this pass declines `func` outright, before any price is taken. The
/// `spliced` lookahead table reads it too, so the table cannot charge a caller
/// for a body that will stay a call.
fn splice_barred(
    func: &NirFunction,
    recursive_functions: &IndexSet<FuncId>,
    type_table: &TypeTable,
) -> bool {
    // A CM binding is an ABI bridge between Wado GC types and CM linear memory,
    // and has to stay a function of its own.
    if func.inline_hint == InlineHint::Never || func.is_cm_binding {
        return true;
    }
    // Recursion is barred ahead of the `#[inline(always)]` short-circuit below:
    // splicing a recursive call only exposes the next one, so a force would
    // expand without bound over the fixed point (a compiler stack overflow at
    // higher iteration counts). Keyed on `FuncId`, the identity the recursive set
    // is built on, so a cross-module recursive function is not missed.
    if func.id.is_some_and(|id| recursive_functions.contains(&id)) {
        return true;
    }
    // A `!`-returning body is an error/abort path: never hot, nothing to gain.
    // `#[inline(always)]` overrides this one, which is why it is not a bare test.
    func.inline_hint != InlineHint::Always && type_table.is_never(func.return_type)
}

/// The threshold `func`'s own hint puts it under. An `#[inline]` hint raises it
/// 5x.
///
/// The threshold applies even at a single call site: if that site sits inside a
/// function itself duplicated at N sites, the large callee is copied N times
/// rather than shared. Bypassing it measured +87% (`pi_approx`) / +186% (zlib) at
/// `-Os` and regressed already at `-O1`.
fn effective_threshold(func: &NirFunction, inline_threshold: usize) -> usize {
    if func.inline_hint == InlineHint::Hint {
        inline_threshold * 5
    } else {
        inline_threshold
    }
}

/// Decide a callee's fate: whether to splice it, and whether to leave it
/// alone so it stays spliceable. Both answers come from here so they cannot
/// disagree about the budget or about which callees are eligible at all.
fn classify_callee(
    func: &NirFunction,
    const_view: Option<&ConstView<'_>>,
    recursive_functions: &IndexSet<FuncId>,
    type_table: &TypeTable,
    inline_threshold: usize,
    descriptors: &[FunctionRef],
    foldable: &[bool],
    loopy: &[bool],
    safepoint_calls: &[bool],
    sites: usize,
    spliced: &[usize],
    hopeful: &IndexSet<u32>,
) -> Verdict {
    // Must have a body
    let Some(body) = &func.body else {
        return Verdict::default();
    };

    if splice_barred(func, recursive_functions, type_table) {
        return Verdict::default();
    }

    // #[inline(always)] skips the remaining heuristic checks (but still requires
    // a body and passes `splice_barred`, both checked above)
    if func.inline_hint == InlineHint::Always {
        return Verdict {
            hot: 0,
            reach: Reach::Everywhere,
            hold: false,
        };
    }

    let effective_threshold = effective_threshold(func, inline_threshold);

    let params = func.params.len();
    let plain = inline_cost(body, type_table, descriptors, spliced);
    if net_cost(plain, params) <= effective_threshold {
        return Verdict {
            hot: plain,
            reach: Reach::Everywhere,
            hold: false,
        };
    }

    // Over budget as written, but the caller's constants may still fold it
    // under. `(fits, drops a loop)` for one reading of the body.
    let weigh = |view: &ConstView<'_>| {
        let folded = inline_cost_folded(body, type_table, descriptors, view, spliced);
        if net_cost(folded, params) > effective_threshold {
            return (false, false);
        }
        let walk = CostWalk::new(body, type_table, descriptors, Price::Hot).under(view);
        (folded * 2 <= plain, fold_drops_loop(body, view, &walk))
    };

    // `inline` reads the call sites, since splicing pays at the sites that
    // exist; the hold reads the body alone, since it protects an option whose
    // evidence has not arrived — a `field<T>` in a derived serializer is
    // admitted at plain=18 against a budget of 13, on a folded price the
    // reflection walk only produces rounds later.
    //
    // Only a deleted loop counts: a constant receiver halves almost any body,
    // and holding on that stopped `String::get_byte_unchecked` reaching a
    // two-line `peek`. Measured slower and not worth retrying — holding what
    // the call sites already admit, holding every candidate, and splicing from
    // a frozen copy, which no other pass can reach and `sroa_param` invalidates.
    // Optimistic about the call sites, but only as far as they leave room: a
    // parameter no site can still hand a constant is a bet already lost, and
    // holding on it only defers the splices into this body to a second
    // convergence of the loop.
    let optimistic_loop = !hopeful.is_empty()
        && weigh(&ConstView {
            params: hopeful,
            foldable,
            loopy,
        })
        .1;
    let folds = const_view.is_some_and(|view| {
        let (halves, drops_loop) = weigh(view);
        halves || drops_loop
    });
    let reach = if folds {
        Reach::Everywhere
    } else if net_cost(plain, params) > effective_threshold * PAYBACK_FACTOR
        || has_safepoint(body, type_table, descriptors, safepoint_calls)
    {
        Reach::Nowhere
    } else if sites == 1 {
        Reach::Sole
    } else {
        Reach::Loops
    };
    Verdict {
        hot: plain,
        reach,
        hold: optimistic_loop,
    }
}

/// Detect recursive functions using call graph analysis.
///
/// Every function's `FuncId` equals its store position in `project.functions`
/// (`FuncId == position`, asserted end-to-end at WIR build), and every call
/// site's stamped `func_id` resolves to that same position. So the call graph
/// is indexed directly by position: a node is a function, its edges are the
/// `func_id.index()` of each callee — no name-keyed identity table, and no
/// dedup that could collapse two distinct functions onto one node.
pub(super) fn find_recursive_functions(functions: &[Rc<RefCell<NirFunction>>]) -> IndexSet<FuncId> {
    let n = functions.len();
    let mut call_graph: Vec<Vec<usize>> = vec![Vec::new(); n];

    for (i, func_rc) in functions.iter().enumerate() {
        let func = func_rc.borrow();
        if let Some(body) = &func.body {
            let mut callee_ids: IndexSet<usize> = IndexSet::default();
            collect_callees(body, &mut callee_ids);
            call_graph[i] = callee_ids.into_iter().collect();
        }
    }
    recursive_functions(&call_graph)
}

/// The functions on a call cycle of `call_graph`, which holds each function's
/// callees by store position.
fn recursive_functions(call_graph: &[Vec<usize>]) -> IndexSet<FuncId> {
    // A function is recursive iff it lies on a call cycle — i.e. it is a member
    // of a non-trivial strongly-connected component, or has a self-edge. One
    // iterative Tarjan pass computes every SCC in O(V + E), versus the old
    // per-function reachability DFS at O(V·(V + E)).
    let recursive_idx = recursive_scc_members(call_graph);
    (0..call_graph.len())
        .filter(|&i| recursive_idx[i])
        .map(FuncId::new)
        .collect()
}

/// Iterative Tarjan SCC. Returns one bool per node: `true` when the node lies on
/// a call cycle (a non-singleton SCC member, or a node with a self-edge).
pub(super) fn recursive_scc_members(call_graph: &[Vec<usize>]) -> Vec<bool> {
    let n = call_graph.len();
    const UNVISITED: usize = usize::MAX;
    let mut index_of = vec![UNVISITED; n];
    let mut lowlink = vec![0usize; n];
    let mut on_stack = vec![false; n];
    let mut scc_stack: Vec<usize> = Vec::new();
    let mut recursive = vec![false; n];
    let mut next_index = 0usize;
    // Explicit DFS stack: (node, next child position).
    let mut work: Vec<(usize, usize)> = Vec::new();

    for start in 0..n {
        if index_of[start] != UNVISITED {
            continue;
        }
        work.push((start, 0));
        while let Some(&(v, ci)) = work.last() {
            if ci == 0 {
                index_of[v] = next_index;
                lowlink[v] = next_index;
                next_index += 1;
                scc_stack.push(v);
                on_stack[v] = true;
            }
            if ci < call_graph[v].len() {
                let w = call_graph[v][ci];
                work.last_mut().unwrap().1 += 1;
                if index_of[w] == UNVISITED {
                    work.push((w, 0));
                } else if on_stack[w] {
                    lowlink[v] = lowlink[v].min(index_of[w]);
                }
                continue;
            }
            // All of `v`'s children are done. If `v` roots an SCC, pop it.
            if lowlink[v] == index_of[v] {
                let mut size = 0usize;
                loop {
                    let w = scc_stack.pop().unwrap();
                    on_stack[w] = false;
                    recursive[w] = true;
                    size += 1;
                    if w == v {
                        break;
                    }
                }
                // A singleton SCC is only recursive through a self-edge.
                if size == 1 && !call_graph[v].contains(&v) {
                    recursive[v] = false;
                }
            }
            work.pop();
            if let Some(&(parent, _)) = work.last() {
                lowlink[parent] = lowlink[parent].min(lowlink[v]);
            }
        }
    }
    recursive
}

/// The callee of every `Call` site under `node`, once per site.
fn for_each_call_site(body: &Body, node: NodeRef, mut f: impl FnMut(FuncId)) {
    body.for_each_live_node_under(node, |n| {
        if let NodeRef::Expr(id) = n
            && let ExprKind::Call { func_id, .. } = &body.exprs[id].kind
        {
            f(*func_id);
        }
    });
}

/// Collect the store position (`func_id.index()`) of every `Call` callee
/// reachable in `body` (order is irrelevant — the result is a set feeding the
/// recursion call graph). Each stamped `func_id` is total and resolves to a
/// position in `project.functions`, which is exactly the call-graph node index.
fn collect_callees(body: &Body, callees: &mut IndexSet<usize>) {
    for_each_call_site(body, NodeRef::Block(body.root), |callee| {
        callees.insert(callee.index());
    });
}

/// How many call sites each callee has, counted with repetition and keyed by
/// store position. This is the multiplier on what one splice adds, so a callee
/// reached twice from one body counts twice — unlike [`collect_callees`], whose
/// answer is a set because the recursion graph only asks whether an edge exists.
fn call_site_counts(scans: &[BodyScan]) -> Vec<usize> {
    let mut counts = vec![0usize; scans.len()];
    for (callee, _) in scans.iter().flat_map(|s| &s.calls) {
        counts[callee.index()] += 1;
    }
    counts
}

/// A callee the heuristic admitted, priced for the budget.
struct Candidate {
    id: FuncId,
    name: String,
    /// What running one copy costs — the price the threshold judged.
    hot: usize,
    /// What holding one copy costs. Above `hot` exactly when the body keeps a
    /// cold path the threshold discounted.
    size: usize,
    sites: usize,
    /// `#[inline(always)]`: a directive, so it is charged but never declined.
    forced: bool,
}

/// A callee the budget turned down, for the post-loop report.
pub(super) struct Declined {
    name: String,
    hot: usize,
    size: usize,
    sites: usize,
}

/// How far the inliner may push the package past the size it entered the
/// optimizer loop at.
///
/// The threshold decides whether one copy is worth making; this decides how many
/// copies the package can afford. One number cannot do both jobs, and a
/// threshold asked to do both ends up tuned to sit just under whichever
/// program's cliff was last measured.
#[derive(Default)]
pub(super) struct InlineBudget {
    /// Percent of the baseline the inliner may add on top of it, or `None` for
    /// a level that does not bound it.
    growth: Option<u32>,
    /// Unit size the first round saw. Anchoring here rather than on the current
    /// size is what makes the budget finite: the loop runs many rounds, and a
    /// cap re-read from a grown unit would ratchet.
    baseline: Option<usize>,
    /// Keyed by callee, so a candidate turned down in several rounds is reported
    /// once, at its latest price. By `FuncId` rather than by name: two
    /// same-named callees in different modules are two candidates, not one.
    declined: IndexMap<FuncId, Declined>,
}

/// The callees [`Verdict::hold`] keeps un-spliced-into, carried across rounds.
///
/// A hold bets on constants a later round brings. A loop that converged with
/// the hold still in place brings none, so [`Self::release`] settles the bet
/// and lets the function receive inlining like any other.
#[derive(Default)]
pub(super) struct InlineHolds {
    held: IndexSet<FuncId>,
    released: bool,
}

impl InlineHolds {
    /// Record this round's verdict on `id`. A held function is skipped as a
    /// caller but still marked seen, so a hold that lapses on its own — a
    /// callee's price moved without touching this function's revision — has to
    /// re-dirty it, or it would receive nothing to the end of the loop.
    fn settle(&mut self, id: FuncId, hold: bool, gate: &mut FunctionGate) {
        if hold && !self.released {
            self.held.insert(id);
        } else if self.held.swap_remove(&id) {
            gate.mark_changed(id);
        }
    }

    /// Release every hold, marking those functions for another round. The
    /// drain and the [`Self::settle`] guard leave the set empty for good, so
    /// the call after the reopened run converges is the loop's exit.
    pub(super) fn release(&mut self, gate: &mut FunctionGate) -> bool {
        if self.held.is_empty() {
            return false;
        }
        self.released = true;
        for id in self.held.drain(..) {
            gate.mark_changed(id);
        }
        true
    }

    pub(super) fn still_held(&self) -> usize {
        self.held.len()
    }
}

/// Below this the budget does not apply. A small package has no cliff to fall
/// off — the absolute growth is bounded by how little there is to copy — and a
/// percentage of a small baseline is too tight to inline anything at all.
const BUDGET_MIN_UNIT: usize = 4_000;

impl InlineBudget {
    pub(super) fn new(growth: Option<u32>) -> Self {
        Self {
            growth,
            ..Default::default()
        }
    }

    /// Whether anything reads a round's prices: a cap to spend them against, or
    /// a trace that reports them. Taking them walks every body in the unit, so
    /// the default — no cap, no trace — should not pay for it.
    pub(super) fn prices_read(&self) -> bool {
        let trace = filter();
        self.growth.is_some() || trace.enabled("inline") || trace.enabled("opt_loop")
    }

    /// What the inliner may still add, given the unit's size right now. Read
    /// from the live size rather than accumulated per round, so a round that
    /// over-estimated its growth — or a later pass that folded some away — hands
    /// the difference back instead of losing it.
    fn headroom(&mut self, unit_size: usize) -> Option<usize> {
        let growth = self.growth? as usize;
        let baseline = *self.baseline.get_or_insert(unit_size);
        if baseline < BUDGET_MIN_UNIT {
            return None;
        }
        Some((baseline + baseline * growth / 100).saturating_sub(unit_size))
    }

    /// Spend this round's headroom over `priced` and return the candidates it
    /// could not cover.
    ///
    /// Cheapest body first: with no profile to say which callee is hot, the
    /// ranking that is defensible is the one that buys the most inlining per
    /// unit of budget. A single-site candidate costs nothing (its body goes with
    /// the site), so it is never what runs the budget out.
    fn select(&mut self, priced: &mut [Candidate], unit_size: usize) -> IndexSet<FuncId> {
        let mut over = IndexSet::default();
        let Some(headroom) = self.headroom(unit_size) else {
            return over;
        };
        priced.sort_by_key(|c| c.size);
        let mut spent = 0usize;
        for c in priced {
            let growth = splice_growth(c.size, c.sites);
            if c.forced || spent + growth <= headroom {
                spent += growth;
                continue;
            }
            over.insert(c.id);
            self.declined.insert(
                c.id,
                Declined {
                    name: c.name.clone(),
                    hot: c.hot,
                    size: c.size,
                    sites: c.sites,
                },
            );
        }
        over
    }

    /// What the budget turned down over the whole run, largest first. Both
    /// prices are named: a `hot` far under `size` is a callee the threshold
    /// admitted on a cold-discounted price and the budget then had to pay for
    /// in full.
    pub(super) fn report(&self) -> Option<String> {
        let (growth, baseline) = (self.growth?, self.baseline?);
        let largest = self.declined.values().max_by_key(|d| d.size * d.sites)?;
        Some(format!(
            "inline: the {}% growth budget over {} declined {} candidate(s); largest `{}` (hot {}, size {} x {} sites)",
            growth,
            baseline,
            self.declined.len(),
            largest.name,
            largest.hot,
            largest.size,
            largest.sites,
        ))
    }
}

/// What splicing one callee everywhere adds to the unit: a copy per call site,
/// less the body itself, which goes with the last site that called it.
fn splice_growth(size: usize, sites: usize) -> usize {
    size * sites.saturating_sub(1)
}

/// The callees this round splices, and what the re-scan of one splice may still
/// add to it.
#[derive(Clone, Copy)]
struct Candidates<'a> {
    bodies: &'a IndexMap<FuncId, NirFunction>,
    /// Which sites splice each candidate.
    reach: &'a IndexMap<FuncId, Reach>,
    /// Per candidate, the call sites a splice of it brings, its own splices'
    /// included: what a loop holding a site of it holds once the round ends.
    carried: &'a IndexMap<FuncId, IndexMap<FuncId, usize>>,
    /// Each candidate's written price, net of the call site it replaces.
    net_price: &'a IndexMap<FuncId, usize>,
    rescan_cap: usize,
    /// What the re-scan under way may still splice, or `None` outside one.
    rescan_left: Option<&'a Cell<usize>>,
}

impl<'a> Candidates<'a> {
    /// Spend `id`'s price from the re-scan under way, if there is one and it
    /// can pay.
    fn charge(&self, id: FuncId) -> bool {
        let Some(left) = self.rescan_left else {
            return true;
        };
        let price = self.net_price[&id];
        if price > left.get() {
            return false;
        }
        left.set(left.get() - price);
        true
    }

    /// Inside a fresh splice: charged to the re-scan under way, or to `left`
    /// when this splice starts one.
    fn rescanning<'b>(self, left: &'b Cell<usize>) -> Candidates<'b>
    where
        'a: 'b,
    {
        Candidates {
            rescan_left: Some(self.rescan_left.unwrap_or(left)),
            ..self
        }
    }
}

/// Inline eligible functions at their call sites
///
/// The `inline_threshold` parameter controls the maximum number of statements
/// a function can have to be considered for inlining.
pub fn inline_functions(
    project: &mut NirPackage,
    inline_threshold: usize,
    budget: &mut InlineBudget,
    holds: &mut InlineHolds,
    gate: &mut FunctionGate,
    descriptor_cache: &mut DescriptorCache,
) -> bool {
    // Callee identity by `func_id` (descriptor table built once from the records,
    // borrow-safe), so a call site is recognized by its stamped id rather than the
    // call node's `FunctionRef`. Indexed by `func_id.index()` (== store position).
    let descriptors = descriptor_cache.descriptors(project);
    let scans: Vec<BodyScan> = project
        .functions
        .iter()
        .map(|f| scan_body(&f.borrow()))
        .collect();
    let call_graph: Vec<Vec<usize>> = scans
        .iter()
        .map(|s| s.calls.iter().map(|(c, _)| c.index()).collect())
        .collect();
    let recursive_functions = recursive_functions(&call_graph);

    // Collect inline candidates from all modules, keyed by `FuncId` (the
    // function's store position). A call site resolves its candidate by its
    // stamped `func_id` directly, so the key is the exact callee identity — no
    // `(module, name)` lookup, no entry-point fallback, no collision between two
    // functions that happen to share a name.
    let mut inline_candidates: IndexMap<FuncId, NirFunction> = IndexMap::default();
    let mut reach: IndexMap<FuncId, Reach> = IndexMap::default();
    let mut net_price: IndexMap<FuncId, usize> = IndexMap::default();

    // Also collect function_strings for each candidate (to update caller's
    // strings after inlining). `function_strings` is keyed by `(module, name)`;
    // map each candidate's strings onto its `FuncId` here.
    let mut candidate_strings: IndexMap<FuncId, Vec<String>> = IndexMap::default();

    // Inputs for the folded-cost second chance: which parameters arrive
    // constant everywhere, which callees the compile-time engine runs on
    // constant arguments, and which of those spin a loop while doing it.
    let sites = argument_sites(&scans);
    let const_params = constant_params(project, &scans, &sites);
    let safepoint_calls = safepoint_calls(project, descriptors);
    let fn_effects = compute_fn_effects(project);
    let foldable: Vec<bool> = project
        .functions
        .iter()
        .zip(&fn_effects)
        .map(|(f, e)| e.is_pure() && is_ctfe_eligible(&f.borrow()))
        .collect();
    let loopy: Vec<bool> = scans.iter().map(|s| s.loopy).collect();

    // What the unit holds right now, and what each admitted candidate would add
    // to it. `Candidate::forced` marks the ones the budget may not turn down.
    // Both prices cost a walk over the whole unit per round and no level sets a
    // growth cap, so they are taken only where something reads them. The site
    // counts are taken every round, since a callee's only site is one where its
    // splice pays back.
    let pricing = budget.prices_read();
    let call_sites = call_site_counts(&scans);
    let hopeful_params = hopeful_params(project, &scans, &sites);
    let mut unit_size = 0usize;
    let mut priced: Vec<Candidate> = Vec::new();

    let type_table = project.type_table.borrow();
    // What a call to each function costs a caller that splices it, for
    // `CostWalk::splicing`. Every price here is read as written, with calls
    // charged as ABI edges, so the table is one level of lookahead and cannot
    // recurse. It keeps `0` for a function this pass leaves alone, reading
    // `splice_barred` / `effective_threshold` so it agrees with
    // `classify_callee` on which those are.
    //
    // The two disagree on one shape: this admission test reads the gross price,
    // while `classify_callee` reads the price this table produces, so a driver
    // that its own loopy callees push over the threshold is declined there and
    // charged to its callers here. Closing that takes a second lookahead level.
    //
    // Only a callee carrying a loop is priced this way. A loop is what the model
    // charges most to splice, being a region of its own in the caller. Pricing
    // every inlinable callee by its whole body judges a driver by its
    // post-inlining size against a threshold calibrated on as-written ones, and
    // that suppresses inlining the CBOR serializers need.
    let spliced: Vec<usize> = project
        .functions
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let func = f.borrow();
            let Some(body) = func.body.as_ref() else {
                return 0;
            };
            if !loopy.get(i).copied().unwrap_or(false)
                || splice_barred(&func, &recursive_functions, &type_table)
            {
                return 0;
            }
            let gross = inline_cost(body, &type_table, descriptors, &[]);
            if func.inline_hint == InlineHint::Always
                || net_cost(gross, func.params.len())
                    <= effective_threshold(&func, inline_threshold)
            {
                gross
            } else {
                0
            }
        })
        .collect();
    for (i, func_rc) in project.functions.iter().enumerate() {
        let func = func_rc.borrow();
        let size = match func.body.as_ref() {
            Some(b) if pricing => inline_size(b, &type_table, descriptors),
            _ => 0,
        };
        unit_size += size;
        let view = func
            .id
            .and_then(|id| const_params.get(&id))
            .map(|params| ConstView {
                params,
                foldable: &foldable,
                loopy: &loopy,
            });
        let verdict = classify_callee(
            &func,
            view.as_ref(),
            &recursive_functions,
            &type_table,
            inline_threshold,
            descriptors,
            &foldable,
            &loopy,
            &safepoint_calls,
            call_sites[i],
            &spliced,
            &hopeful_params[i],
        );
        if let Some(id) = func.id {
            holds.settle(id, verdict.hold, gate);
        }
        if verdict.reach != Reach::Nowhere {
            let id = func.id.expect("func_id assigned at lower");
            reach.insert(id, verdict.reach);
            let string_key = (func.module_source.clone(), func.name.clone());
            // Get the strings used by this function
            if let Some(strings) = project.function_strings.get(&string_key) {
                candidate_strings.insert(id, strings.clone());
            }
            if pricing {
                priced.push(Candidate {
                    id,
                    name: func.name.clone(),
                    hot: verdict.hot,
                    size,
                    sites: call_sites[id.index()],
                    forced: func.inline_hint == InlineHint::Always,
                });
            }
            let hot = match func.inline_hint {
                InlineHint::Always => inline_cost(
                    func.body.as_ref().expect("a candidate has a body"),
                    &type_table,
                    descriptors,
                    &spliced,
                ),
                InlineHint::Auto | InlineHint::Hint | InlineHint::Never => verdict.hot,
            };
            net_price.insert(id, net_cost(hot, func.params.len()));
            inline_candidates.insert(id, func.clone());
        }
    }
    drop(type_table);

    compiler_trace!("inline", "held: [{}]", {
        holds
            .held
            .iter()
            .map(|id| project.functions[id.index()].borrow().name.clone())
            .collect::<Vec<_>>()
            .join(", ")
    });

    // What this round would add, and which callees dominate it. A candidate the
    // threshold admitted only because the cold discount put its hot path under
    // it is flagged `cold`: the two prices disagree about exactly those.
    compiler_trace!("inline", "{}", {
        let mut ranked: Vec<&Candidate> = priced.iter().collect();
        ranked.sort_by_key(|c| std::cmp::Reverse(splice_growth(c.size, c.sites)));
        let total: usize = ranked.iter().map(|c| splice_growth(c.size, c.sites)).sum();
        use std::fmt::Write as _;
        let top = ranked.iter().take(6).fold(String::new(), |mut s, c| {
            let _ = write!(
                s,
                "\n    {:>8}  {} (hot {}, size {} x {} sites){}",
                splice_growth(c.size, c.sites),
                c.name,
                c.hot,
                c.size,
                c.sites,
                if c.hot <= inline_threshold && c.size > inline_threshold {
                    "  [cold]"
                } else {
                    ""
                }
            );
            s
        });
        format!(
            "unit {unit_size}: {} candidates worth {total} of growth{top}",
            priced.len(),
        )
    });

    let over_budget = budget.select(&mut priced, unit_size);
    for id in &over_budget {
        inline_candidates.shift_remove(id);
        candidate_strings.shift_remove(id);
    }

    compiler_trace!(
        "opt_loop",
        "inline: threshold={} candidates={} (unit {}, {} over budget)",
        inline_threshold,
        inline_candidates.len(),
        unit_size,
        over_budget.len()
    );

    if inline_candidates.is_empty() {
        return false;
    }

    let mut changed = false;

    // Purity inputs for the graph-preserving inline gate (the splice site below):
    // an inlined call that mutates no caller-reachable state lets the caller's
    // `value_of` survive the splice. Computed once over the project; the
    // per-call `pure_calls` set is taken per body just before inlining it.
    let inline_first_param_types = first_param_types(project);
    let inline_type_table = project.type_table.borrow();
    let inline_call_immutability = CallImmutability::new(project, &inline_type_table);
    let carried = carried_calls(&inline_candidates);
    let candidates = Candidates {
        bodies: &inline_candidates,
        reach: &reach,
        carried: &carried,
        net_price: &net_price,
        // A threshold's worth of threshold-sized callees: a call tree that
        // doubles per level exceeds it within a few levels.
        rescan_cap: inline_threshold * inline_threshold,
        rescan_left: None,
    };

    // Inline at call sites.
    for fid in gate.dirty_funcs(GatedPass::Inline, project.functions.len()) {
        if holds.held.contains(&fid) {
            continue;
        }
        let caller_idx = fid.index();
        let func_rc = project.functions[caller_idx].clone();
        let mut func = func_rc.borrow_mut();
        let caller_module_source = func.module_source.clone();
        let func_name = func.name.clone();
        if func.body.is_some() {
            // Track which functions (by `FuncId`) were inlined into this function
            let mut inlined_funcs: Vec<FuncId> = Vec::new();
            // Splice-point re-valuation records (Method A): one per inlined block.
            let mut reval: Vec<InlineRevalInfo> = Vec::new();
            let mut frame = CallerFrame {
                local_count: func.local_count(),
                locals: std::mem::take(&mut func.locals),
                address_taken: std::mem::take(&mut func.address_taken_locals),
                stores_aliased: std::mem::take(&mut func.stores_aliased_locals),
                loop_calls: Vec::new(),
                original_exprs: func.body.as_ref().expect("checked above").exprs.len(),
            };
            let mut labels = InlineLabels::default();
            // Calls in this body that mutate no caller-reachable state, taken
            // *before* the splice (the call exprs survive as `reval.call_expr`
            // keys). Drives the graph-preserving gate below.
            let pure_set = {
                let body = func.body.as_ref().unwrap();
                call_verdicts(
                    body,
                    &inline_type_table,
                    &inline_first_param_types,
                    &inline_call_immutability,
                )
                .pure
            };
            {
                let body = func.body.as_mut().unwrap();
                let root = body.root;
                inline_calls_in_block(
                    body,
                    root,
                    candidates,
                    descriptors,
                    &mut frame,
                    &project.type_table.borrow(),
                    &mut inlined_funcs,
                    &mut labels,
                    &mut reval,
                    Site::Plain,
                );
            }
            func.locals = frame.locals;
            func.address_taken_locals = frame.address_taken;
            func.stores_aliased_locals = frame.stores_aliased;

            if !inlined_funcs.is_empty() {
                changed = true;
                compiler_trace!("inline_sites", "{func_name} <- [{}]", {
                    inlined_funcs
                        .iter()
                        .map(|id| inline_candidates[id].name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                });
                // The splice restructures the body, staling the persisted graph's
                // `loop_entry_values` (licm's pre-header snapshots — the only
                // value-graph state any consumer still reads, `value_of` having
                // been retired). Keep them only for a graph-preserving splice —
                // every inlined call **pure** (mutates no caller-reachable state)
                // and **loop-free** (introduces no new back-edge) — otherwise clear
                // so licm re-derives conservatively (an absent entry is sound). The
                // value pool and promoted operands carry every value a consumer
                // reads across the splice.
                let preserving = func.body.as_ref().is_some_and(|b| {
                    reval.iter().all(|i| {
                        pure_set.contains(&i.call_expr)
                            && !arena_query::block_contains_loop(b, i.block)
                    })
                });
                if !preserving
                    && let Some(vg) = func.body.as_mut().and_then(|b| b.value_graph.as_mut())
                {
                    vg.loop_entry_values.clear();
                }
                // Only this caller's body changed (callee bodies are copied,
                // not modified), so report just the caller. The caller's
                // call-graph edges shift, but stale edges only cost 1-hop
                // propagation precision (quality), not correctness.
                gate.mark_changed(FuncId::new(caller_idx));
            }

            // Update function_strings: add strings from inlined functions to the caller
            let mut all_inlined_strings: IndexSet<String> = IndexSet::default();
            for inlined_key in inlined_funcs {
                if let Some(inlined_strings) = candidate_strings.get(&inlined_key) {
                    all_inlined_strings.extend(inlined_strings.iter().cloned());
                }
            }
            if !all_inlined_strings.is_empty() {
                // Need to drop func borrow before borrowing project.function_strings mutably
                drop(func);
                {
                    let caller_strings = project
                        .function_strings
                        .entry((caller_module_source.clone(), func_name.clone()))
                        .or_default();
                    let existing: IndexSet<&str> =
                        caller_strings.iter().map(String::as_str).collect();
                    let to_add: Vec<String> = all_inlined_strings
                        .iter()
                        .filter(|s| !existing.contains(s.as_str()))
                        .cloned()
                        .collect();
                    caller_strings.extend(to_add);
                }
                let to_add: Vec<String> = {
                    let existing_literals: IndexSet<&str> =
                        project.string_literals.iter().map(String::as_str).collect();
                    all_inlined_strings
                        .into_iter()
                        .filter(|s| !existing_literals.contains(s.as_str()))
                        .collect()
                };
                project.string_literals.extend(to_add);
            }
        }
    }
    changed
}

/// The caller's local frame, which every splice extends: the locals it gains
/// and the annotations the alias analysis reads about them.
struct CallerFrame {
    local_count: u32,
    locals: Vec<NirLocal>,
    address_taken: IndexSet<u32>,
    stores_aliased: IndexSet<u32>,
    /// Per enclosing loop, innermost last: how many sites in its body call
    /// each function, the ones its splices bring included.
    loop_calls: Vec<IndexMap<FuncId, usize>>,
    /// How many expressions the body held before this pass spliced into it.
    /// The arena only appends, so a call below it is one the round's site
    /// counts saw, and one above it came in with a splice.
    original_exprs: usize,
}

impl CallerFrame {
    /// Whether `call` is a site the round began with.
    fn original_site(&self, call: ExprId) -> bool {
        call.index() < self.original_exprs
    }

    /// Whether `callee` is called from one site of the innermost loop. Several
    /// sites of one callee in a loop are a dispatch, each running on some
    /// iterations only, so the per-iteration saving a [`Site::Loop`] price
    /// counts on is not there, while the growth is paid at every site.
    fn sole_loop_site(&self, callee: FuncId) -> bool {
        self.loop_calls
            .last()
            .is_none_or(|calls| calls.get(&callee).is_none_or(|&n| n <= 1))
    }
}

/// How many sites under `block` call each function, counting the sites a
/// candidate's splice brings as the block's own.
fn call_counts(
    body: &Body,
    block: BlockId,
    carried: &IndexMap<FuncId, IndexMap<FuncId, usize>>,
) -> IndexMap<FuncId, usize> {
    let mut out = IndexMap::default();
    for_each_call_site(body, NodeRef::Block(block), |callee| {
        add_site(&mut out, callee, carried);
    });
    out
}

fn add_site(
    out: &mut IndexMap<FuncId, usize>,
    callee: FuncId,
    carried: &IndexMap<FuncId, IndexMap<FuncId, usize>>,
) {
    *out.entry(callee).or_default() += 1;
    for (&g, &n) in carried.get(&callee).into_iter().flatten() {
        *out.entry(g).or_default() += n;
    }
}

/// [`Candidates::carried`]: per candidate, its body's call sites with each
/// candidate among them expanded into the sites it carries in turn. No
/// candidate reaches itself, since a recursive function is never one.
fn carried_calls(
    candidates: &IndexMap<FuncId, NirFunction>,
) -> IndexMap<FuncId, IndexMap<FuncId, usize>> {
    fn visit(
        id: FuncId,
        candidates: &IndexMap<FuncId, NirFunction>,
        out: &mut IndexMap<FuncId, IndexMap<FuncId, usize>>,
    ) {
        if out.contains_key(&id) {
            return;
        }
        let body = candidates[&id]
            .body
            .as_ref()
            .expect("a candidate has a body");
        let mut callees = Vec::new();
        for_each_call_site(body, NodeRef::Block(body.root), |g| callees.push(g));
        for &g in &callees {
            assert_ne!(g, id, "a candidate is never recursive");
            if candidates.contains_key(&g) {
                visit(g, candidates, out);
            }
        }
        let mut sites = IndexMap::default();
        for g in callees {
            add_site(&mut sites, g, out);
        }
        out.insert(id, sites);
    }
    let mut out = IndexMap::default();
    for &id in candidates.keys() {
        visit(id, candidates, &mut out);
    }
    out
}

/// Inline function calls in a block, each statement processed in place: a
/// `Let` / `Expr` / `Return` value gets a top-level attempt that then re-scans
/// the inlined body, while others recurse. `site` is the context the block's
/// calls sit in; a `cold_path()` marker turns it [`Site::Cold`] for the rest of
/// the block, and a loop body is a [`Site::Loop`].
fn inline_calls_in_block(
    body: &mut Body,
    block: BlockId,
    candidates: Candidates<'_>,
    descriptors: &[FunctionRef],
    frame: &mut CallerFrame,
    type_table: &TypeTable,
    inlined_funcs: &mut Vec<FuncId>,
    labels: &mut InlineLabels,
    reval: &mut Vec<InlineRevalInfo>,
    mut site: Site,
) {
    enum Shape {
        TopLevel(ExprId),
        Nested(ExprId),
        If(Option<ExprId>, BlockId, Option<BlockId>),
        Block(BlockId),
        Loop(BlockId),
        None,
    }
    for stmt_id in body.blocks[block].stmts.clone() {
        if let StmtKind::Expr(Operand::Expr(e)) = &body.stmts[stmt_id].kind
            && is_cold_path_call(body, *e, descriptors)
        {
            site = Site::Cold;
        }
        let shape = match &body.stmts[stmt_id].kind {
            StmtKind::Let { value, .. } => value.as_expr().map_or(Shape::None, Shape::TopLevel),
            StmtKind::Expr(expr) => expr.as_expr().map_or(Shape::None, Shape::TopLevel),
            StmtKind::Return { value: Some(v) } => v.as_expr().map_or(Shape::None, Shape::TopLevel),
            StmtKind::If {
                condition,
                then_block,
                else_block,
            } => Shape::If(condition.as_expr(), *then_block, *else_block),
            StmtKind::Loop { body: b } => Shape::Loop(*b),
            StmtKind::LabeledBlock { block: b, .. } => Shape::Block(*b),
            StmtKind::Break { value: Some(v), .. } => {
                v.as_expr().map_or(Shape::None, Shape::Nested)
            }
            StmtKind::LetDestructure { value, .. } => {
                value.as_expr().map_or(Shape::None, Shape::Nested)
            }
            _ => Shape::None,
        };
        match shape {
            Shape::TopLevel(value) => {
                let new_value = inline_top_level(
                    body,
                    value,
                    candidates,
                    descriptors,
                    frame,
                    type_table,
                    inlined_funcs,
                    labels,
                    reval,
                    site,
                );
                match &mut body.stmts[stmt_id].kind {
                    StmtKind::Let { value, .. } => *value = new_value.into(),
                    StmtKind::Expr(expr) => *expr = new_value.into(),
                    StmtKind::Return { value } => *value = Some(new_value.into()),
                    _ => {}
                }
            }
            Shape::Nested(value) => inline_calls_in_expr(
                body,
                value,
                candidates,
                descriptors,
                frame,
                type_table,
                inlined_funcs,
                labels,
                reval,
                site,
            ),
            Shape::If(cond, tb, eb) => {
                if let Some(cond) = cond {
                    inline_calls_in_expr(
                        body,
                        cond,
                        candidates,
                        descriptors,
                        frame,
                        type_table,
                        inlined_funcs,
                        labels,
                        reval,
                        site,
                    );
                }
                inline_calls_in_block(
                    body,
                    tb,
                    candidates,
                    descriptors,
                    frame,
                    type_table,
                    inlined_funcs,
                    labels,
                    reval,
                    site,
                );
                if let Some(eb) = eb {
                    inline_calls_in_block(
                        body,
                        eb,
                        candidates,
                        descriptors,
                        frame,
                        type_table,
                        inlined_funcs,
                        labels,
                        reval,
                        site,
                    );
                }
            }
            Shape::Block(b) => inline_calls_in_block(
                body,
                b,
                candidates,
                descriptors,
                frame,
                type_table,
                inlined_funcs,
                labels,
                reval,
                site,
            ),
            Shape::Loop(b) => {
                frame
                    .loop_calls
                    .push(call_counts(body, b, candidates.carried));
                inline_calls_in_block(
                    body,
                    b,
                    candidates,
                    descriptors,
                    frame,
                    type_table,
                    inlined_funcs,
                    labels,
                    reval,
                    site.entering_loop(),
                );
                frame.loop_calls.pop();
            }
            Shape::None => {}
        }
    }
}

/// Top-level inline of a statement value: try to inline the call, and if it
/// fires, re-scan the inlined body for nested opportunities, as far as the
/// re-scan cap pays for. Returns the (possibly new) value expression id.
fn inline_top_level(
    body: &mut Body,
    value: ExprId,
    candidates: Candidates<'_>,
    descriptors: &[FunctionRef],
    frame: &mut CallerFrame,
    type_table: &TypeTable,
    inlined_funcs: &mut Vec<FuncId>,
    labels: &mut InlineLabels,
    reval: &mut Vec<InlineRevalInfo>,
    site: Site,
) -> ExprId {
    let result = try_inline_call_expr(
        body, value, candidates, frame, type_table, labels, reval, site,
    );
    if let Some((new_id, inlined_key)) = result {
        if !inlined_funcs.contains(&inlined_key) {
            inlined_funcs.push(inlined_key);
        }
        let left = Cell::new(candidates.rescan_cap);
        inline_calls_in_expr(
            body,
            new_id,
            candidates.rescanning(&left),
            descriptors,
            frame,
            type_table,
            inlined_funcs,
            labels,
            reval,
            site,
        );
        new_id
    } else {
        inline_calls_in_expr(
            body,
            value,
            candidates,
            descriptors,
            frame,
            type_table,
            inlined_funcs,
            labels,
            reval,
            site,
        );
        value
    }
}

/// The expression and block children of `e`, excluding patterns, in the order
/// the tree `inline_calls_in_expr` recursed (expression children first, then
/// block children — `If`/`Switch` put condition/scrutinee before their blocks,
/// so the split preserves visitation order, which drives label / local
/// numbering).
fn inline_expr_children(body: &Body, e: ExprId) -> (Vec<ExprId>, Vec<BlockId>) {
    let mut exprs = Vec::new();
    let mut blocks = Vec::new();
    // `for_each_child` yields expression children before block children for every
    // `ExprKind` (`If`/`Switch` emit condition/scrutinee ahead of their blocks),
    // so splitting into two ordered vecs preserves the exact visitation order the
    // splice's label / local numbering depends on. Pattern / statement children
    // carry no inlinable call in this walk and are skipped.
    body.for_each_child(NodeRef::Expr(e), |c| match c {
        NodeRef::Expr(x) => exprs.push(x),
        NodeRef::Block(b) => blocks.push(b),
        NodeRef::Stmt(_) | NodeRef::Pat(_) => {}
    });
    (exprs, blocks)
}

/// Binding for a single parameter during inlining.
///
/// Each binding becomes a `Let` statement at the head of the synthesized
/// labeled block. Fields carry the information needed without requiring the
/// shared helper to know whether the call site is a free function or a method.
struct InlineBinding {
    /// The callee-frame local index of the parameter.
    callee_local_index: u32,
    /// Parameter name (kept for the synthesized binding `Let`).
    name: String,
    is_mut: bool,
    /// The `Let`'s declared type: the callee parameter's own type, since the
    /// binding stands in for that parameter.
    local_type: TypeId,
    /// The argument operand, already in the caller arena. The call node is
    /// discarded after inlining, so its argument subtrees / pool values are
    /// reused directly.
    value: Operand,
}

/// Threaded context for the callee->caller splice: how to remap the callee's
/// local indices and inner labels, and which label a `return` breaks to.
pub(super) struct InlineCtx<'a> {
    param_to_local: &'a IndexMap<u32, u32>,
    local_offset: u32,
    param_count: u32,
    label: &'a str,
    label_map: &'a IndexMap<String, String>,
}

impl<'a> InlineCtx<'a> {
    /// The context `cold_outline` moves a region under: the first `inherited`
    /// locals keep their indices, each local in `lifted` takes the parameter
    /// slot it maps to, and everything else shifts past the new parameters.
    /// With `lifted` empty this renames nothing.
    pub(super) fn lifting(
        inherited: u32,
        lifted: &'a IndexMap<u32, u32>,
        label_map: &'a IndexMap<String, String>,
    ) -> Self {
        Self {
            param_to_local: lifted,
            local_offset: inherited + lifted.len() as u32,
            param_count: inherited,
            label: "",
            label_map,
        }
    }
}

impl InlineCtx<'_> {
    /// The label a `return` becomes a `break` to. `InlineCtx::lifting` carries
    /// none, since `cold_outline` moves only a region control cannot leave, and
    /// a `break ""` names no block.
    fn return_label(&self) -> String {
        assert!(
            !self.label.is_empty(),
            "[NIR] inline: a `return` reached a splice with no label to break to"
        );
        self.label.to_string()
    }

    pub(super) fn local(&self, idx: u32) -> u32 {
        remap_local_index(
            idx,
            self.param_to_local,
            self.local_offset,
            self.param_count,
        )
    }
    fn lbl(&self, l: &str) -> String {
        self.label_map
            .get(l)
            .cloned()
            .unwrap_or_else(|| l.to_string())
    }
}

/// A spliced inlined block, recorded so the post-splice graph-preserving gate
/// can classify it (the call's purity + whether the block introduces a loop).
pub(super) struct InlineRevalInfo {
    pub block: BlockId,
    /// The original `Call` expr being inlined, keyed against `pure_calls`.
    pub call_expr: ExprId,
}

/// Core inlining routine: builds a labeled block (in the caller arena) that
/// binds each prepared parameter value and executes the spliced callee body
/// with locals remapped into the caller's frame and `return`s converted to
/// `break label`.
fn build_inlined_labeled_block(
    caller: &mut Body,
    candidate: &NirFunction,
    callee: &Body,
    func_name: &str,
    bindings: Vec<InlineBinding>,
    call_span: Span,
    call_expr: ExprId,
    frame: &mut CallerFrame,
    labels: &mut InlineLabels,
    reval: &mut Vec<InlineRevalInfo>,
) -> ExprId {
    let label = labels.fresh(caller, func_name);

    let local_offset = frame.local_count;
    let callee_param_count = candidate.params.len() as u32;
    let callee_local_count = candidate.local_count();
    let new_locals_needed = callee_local_count.saturating_sub(callee_param_count);

    let mut block_stmts: Vec<StmtId> = Vec::with_capacity(bindings.len());
    let mut param_to_local: IndexMap<u32, u32> = IndexMap::default();

    for (i, binding) in bindings.into_iter().enumerate() {
        let new_local_index = local_offset + i as u32;
        param_to_local.insert(binding.callee_local_index, new_local_index);
        frame.locals.push(NirLocal {
            name: binding.name.clone(),
            type_id: binding.local_type,
            is_mut: binding.is_mut,
        });
        frame.local_count += 1;
        let let_id = caller.stmts.push(StmtNode {
            kind: StmtKind::Let {
                name: binding.name,
                local_index: new_local_index,
                is_mut: binding.is_mut,
                is_reactive: false,
                type_id: binding.local_type,
                value: binding.value,
                skip_value_copy: false,
            },
            span: call_span,
        });
        block_stmts.push(let_id);
    }

    let param_offset = local_offset + callee_param_count;
    for i in callee_param_count..callee_local_count {
        if let Some(callee_local) = candidate.locals.get(i as usize) {
            frame.locals.push(callee_local.clone());
        }
    }
    frame.local_count += new_locals_needed;

    // An annotation names a local and the splice renumbers locals. Dropping one
    // lets a read of a boxed local forward past a write through the alias.
    let carry =
        |local: &u32| remap_local_index(*local, &param_to_local, param_offset, callee_param_count);
    frame
        .address_taken
        .extend(candidate.address_taken_locals.iter().map(carry));
    frame
        .stores_aliased
        .extend(candidate.stores_aliased_locals.iter().map(carry));

    let mut inner_labels: IndexSet<String> = IndexSet::default();
    collect_inner_labels(callee, NodeRef::Block(callee.root), &mut inner_labels);
    let mut label_map: IndexMap<String, String> = IndexMap::default();
    for inner_label in inner_labels {
        label_map.insert(inner_label.clone(), format!("{label}__{inner_label}"));
    }

    let ctx = InlineCtx {
        param_to_local: &param_to_local,
        local_offset: param_offset,
        param_count: callee_param_count,
        label: &label,
        label_map: &label_map,
    };
    splice_block_into(caller, callee, callee.root, &ctx, &mut block_stmts);

    let result_type = candidate.return_type;
    let bid = caller.blocks.push(BlockNode {
        stmts: block_stmts,
        span: call_span,
    });
    reval.push(InlineRevalInfo {
        block: bid,
        call_expr,
    });
    caller.exprs.push(ExprNode {
        kind: ExprKind::LabeledBlock {
            label,
            block: bid,
            result_type,
            role: BlockRole::Plain,
        },
        type_id: result_type,
        span: call_span,
    })
}

/// Try to inline the call at `call_id` in `caller`, splicing the callee body in
/// place. Returns the new (labeled-block) expression id and the callee key, or
/// `None` if the call is not an inline candidate. A method binds `self` from
/// `args[0]`.
fn try_inline_call_expr(
    caller: &mut Body,
    call_id: ExprId,
    candidates: Candidates<'_>,
    frame: &mut CallerFrame,
    type_table: &TypeTable,
    labels: &mut InlineLabels,
    reval: &mut Vec<InlineRevalInfo>,
    site: Site,
) -> Option<(ExprId, FuncId)> {
    let ExprKind::Call { func_id, .. } = caller.exprs[call_id].kind else {
        return None;
    };
    // The call's stamped `func_id` is the exact callee identity; look the
    // candidate up directly (no `(module, name)` resolution).
    let candidate = candidates.bodies.get(&func_id)?;
    let admitted = match (site, candidates.reach[&func_id]) {
        (Site::Cold, _) => candidate.inline_hint == InlineHint::Always,
        (Site::Plain | Site::Loop, Reach::Everywhere) => true,
        (Site::Plain | Site::Loop, Reach::Sole) => frame.original_site(call_id),
        (Site::Loop, Reach::Loops) => frame.sole_loop_site(func_id),
        (Site::Plain, Reach::Loops) => false,
        (Site::Plain | Site::Loop, Reach::Nowhere) => {
            unreachable!("a candidate is reached somewhere")
        }
    };
    if !admitted {
        return None;
    }
    if !candidates.charge(func_id) {
        return None;
    }
    let callee = candidate.body.as_ref()?;
    let call_span = caller.exprs[call_id].span;
    let ExprKind::Call { args, .. } = &caller.exprs[call_id].kind else {
        unreachable!("matched a Call above")
    };
    let args = args.clone();

    // Args are already in the caller arena (operands of the discarded call); bind
    // each to its param `Let` directly.
    //
    // The `Let` stands in for the parameter, so it takes the parameter's
    // declared type (`TypeId`s are package-wide after link). The argument's
    // would propagate whatever the caller recorded, including the unresolved
    // type of a synthesized default.
    let mut params = candidate.params.iter();
    let (receiver, rest) = args.split();
    let mut bindings: Vec<InlineBinding> = Vec::with_capacity(candidate.params.len());
    if let Some(receiver) = receiver {
        let self_param = params.next()?;
        let receiver_op = receiver.expr;
        // Bind the receiver to `self`. For `&mut self`, wrap it in a `MutRef` so
        // field mutations write back to the original (the receiver is then an
        // lvalue `Expr`, never a promoted constant); for `&self` / by-value pass
        // the operand directly. The binding takes the receiver's own type where
        // no wrap is needed, since the wrap is what would retype it.
        let recv_type = caller.operand_type(receiver_op);
        let (self_type_id, self_value): (TypeId, Operand) =
            if matches!(type_table.get(self_param.type_id), ResolvedType::MutRef(_))
                && !matches!(type_table.get(recv_type), ResolvedType::MutRef(_))
            {
                let mr = caller.exprs.push(ExprNode {
                    kind: ExprKind::Unary {
                        op: NirUnaryOp::MutRef,
                        expr: receiver_op,
                    },
                    type_id: self_param.type_id,
                    span: call_span,
                });
                (self_param.type_id, mr.into())
            } else {
                (recv_type, receiver_op)
            };
        bindings.push(InlineBinding {
            callee_local_index: self_param.local_index,
            name: self_param.name.clone(),
            is_mut: self_param.is_mut,
            local_type: self_type_id,
            value: self_value,
        });
    }
    bindings.extend(params.zip(rest).map(|(param, arg)| InlineBinding {
        callee_local_index: param.local_index,
        name: param.name.clone(),
        is_mut: param.is_mut,
        local_type: param.type_id,
        value: arg.expr,
    }));

    let inlined = build_inlined_labeled_block(
        caller,
        candidate,
        callee,
        &candidate.name,
        bindings,
        call_span,
        call_id,
        frame,
        labels,
        reval,
    );
    Some((inlined, func_id))
}

/// Remap a local index from the callee frame into the caller frame.
fn remap_local_index(
    index: u32,
    param_to_local: &IndexMap<u32, u32>,
    local_offset: u32,
    param_count: u32,
) -> u32 {
    if let Some(&new_index) = param_to_local.get(&index) {
        return new_index;
    }
    if index >= param_count {
        local_offset + (index - param_count)
    } else {
        index
    }
}

/// Splice the statements of callee `block` into `out` (caller statement ids),
/// converting `return` to `break label` and flattening labeled blocks whose
/// label is never broken to (safe because all locals are uniquely remapped).
fn splice_block_into(
    caller: &mut Body,
    callee: &Body,
    block: BlockId,
    ctx: &InlineCtx,
    out: &mut Vec<StmtId>,
) {
    for sid in callee.blocks[block].stmts.clone() {
        match &callee.stmts[sid].kind {
            StmtKind::Return { value } => {
                let v = *value;
                let span = callee.stmts[sid].span;
                let value = v.map(|x| splice_operand(caller, callee, x, ctx));
                out.push(caller.stmts.push(StmtNode {
                    kind: StmtKind::Break {
                        label: Some(ctx.return_label()),
                        value,
                    },
                    span,
                }));
            }
            StmtKind::LabeledBlock {
                label: inner_label,
                block: inner,
                role,
            } => {
                let (inner_label, role) = (inner_label.clone(), *role);
                let inner = *inner;
                if arena_query::has_break_to(callee, NodeRef::Block(inner), &inner_label) {
                    // The label is broken to, so the block must survive (with its
                    // label remapped); recurse converting returns inside it.
                    let span = callee.stmts[sid].span;
                    let nb = splice_block(caller, callee, inner, ctx);
                    out.push(caller.stmts.push(StmtNode {
                        kind: StmtKind::LabeledBlock {
                            label: ctx.lbl(&inner_label),
                            block: nb,
                            role,
                        },
                        span,
                    }));
                } else {
                    // No break targets this label: flatten its statements into the
                    // parent (all locals are uniquely remapped, so scoping is moot).
                    splice_block_into(caller, callee, inner, ctx, out);
                }
            }
            _ => {
                let s = splice_stmt(caller, callee, sid, ctx);
                out.push(s);
            }
        }
    }
}

/// Splice a callee block into a fresh caller block id (return-converting).
fn splice_block(caller: &mut Body, callee: &Body, block: BlockId, ctx: &InlineCtx) -> BlockId {
    let span = callee.blocks[block].span;
    let mut out = Vec::new();
    splice_block_into(caller, callee, block, ctx, &mut out);
    caller.blocks.push(BlockNode { stmts: out, span })
}

pub(super) fn splice_stmt(
    caller: &mut Body,
    callee: &Body,
    sid: StmtId,
    ctx: &InlineCtx,
) -> StmtId {
    let span = callee.stmts[sid].span;
    let kind = match &callee.stmts[sid].kind {
        StmtKind::Let {
            name,
            local_index,
            is_mut,
            is_reactive,
            type_id,
            value,
            skip_value_copy,
        } => {
            let (li, v) = (*local_index, *value);
            let (name, is_mut, is_reactive, type_id, scv) = (
                name.clone(),
                *is_mut,
                *is_reactive,
                *type_id,
                *skip_value_copy,
            );
            StmtKind::Let {
                name,
                local_index: ctx.local(li),
                is_mut,
                is_reactive,
                type_id,
                value: splice_operand(caller, callee, v, ctx),
                skip_value_copy: scv,
            }
        }
        StmtKind::Expr(e) => StmtKind::Expr(splice_operand(caller, callee, *e, ctx)),
        StmtKind::Return { value } => {
            let v = *value;
            StmtKind::Break {
                label: Some(ctx.return_label()),
                value: v.map(|x| splice_operand(caller, callee, x, ctx)),
            }
        }
        StmtKind::If {
            condition,
            then_block,
            else_block,
        } => {
            let (c, t, e) = (*condition, *then_block, *else_block);
            StmtKind::If {
                condition: splice_operand(caller, callee, c, ctx),
                then_block: splice_block(caller, callee, t, ctx),
                else_block: e.map(|b| splice_block(caller, callee, b, ctx)),
            }
        }
        StmtKind::Loop { body } => {
            let b = *body;
            StmtKind::Loop {
                body: splice_block(caller, callee, b, ctx),
            }
        }
        StmtKind::LabeledBlock { label, block, role } => {
            let (l, b, role) = (label.clone(), *block, *role);
            StmtKind::LabeledBlock {
                label: ctx.lbl(&l),
                block: splice_block(caller, callee, b, ctx),
                role,
            }
        }
        StmtKind::Break { label, value } => {
            let (l, v) = (label.clone(), *value);
            StmtKind::Break {
                label: l.map(|x| ctx.lbl(&x)),
                value: v.map(|x| splice_operand(caller, callee, x, ctx)),
            }
        }
        StmtKind::Continue => StmtKind::Continue,
        StmtKind::LetDestructure { pattern, value } => {
            let (p, v) = (*pattern, *value);
            StmtKind::LetDestructure {
                pattern: splice_pat(caller, callee, p, ctx),
                value: splice_operand(caller, callee, v, ctx),
            }
        }
    };
    caller.stmts.push(StmtNode { kind, span })
}

fn splice_pat(caller: &mut Body, callee: &Body, pid: PatId, ctx: &InlineCtx) -> PatId {
    let span = callee.pats[pid].span;
    let kind = match &callee.pats[pid].kind {
        PatKind::Binding {
            name,
            local_index,
            type_id,
        } => PatKind::Binding {
            name: name.clone(),
            local_index: ctx.local(*local_index),
            type_id: *type_id,
        },
        PatKind::Tuple(ps, rest) => {
            let (ps, rest) = (ps.clone(), *rest);
            PatKind::Tuple(
                ps.into_iter()
                    .map(|p| splice_pat(caller, callee, p, ctx))
                    .collect(),
                rest,
            )
        }
        PatKind::Or(ps) => {
            let ps = ps.clone();
            PatKind::Or(
                ps.into_iter()
                    .map(|p| splice_pat(caller, callee, p, ctx))
                    .collect(),
            )
        }
        PatKind::Variant {
            enum_type,
            variant_name,
            case_index,
            bindings,
            payload_type,
        } => {
            let (et, vn, ci, bs, pt) = (
                *enum_type,
                variant_name.clone(),
                *case_index,
                bindings.clone(),
                *payload_type,
            );
            PatKind::Variant {
                enum_type: et,
                variant_name: vn,
                case_index: ci,
                bindings: bs
                    .into_iter()
                    .map(|p| splice_pat(caller, callee, p, ctx))
                    .collect(),
                payload_type: pt,
            }
        }
        PatKind::Struct {
            struct_type,
            fields,
            has_rest,
        } => {
            let (st, fs, hr) = (*struct_type, fields.clone(), *has_rest);
            PatKind::Struct {
                struct_type: st,
                fields: fs
                    .into_iter()
                    .map(|f| ArenaStructPatternField {
                        field_name: f.field_name,
                        field_index: f.field_index,
                        pattern: splice_pat(caller, callee, f.pattern, ctx),
                    })
                    .collect(),
                has_rest: hr,
            }
        }
        PatKind::ConstantValue { expr } => {
            let e = *expr;
            PatKind::ConstantValue {
                expr: splice_operand(caller, callee, e, ctx),
            }
        }
        PatKind::Wildcard => PatKind::Wildcard,
        PatKind::Literal(l) => PatKind::Literal(l.clone()),
        PatKind::Enum {
            enum_type,
            case_name,
            case_index,
        } => PatKind::Enum {
            enum_type: *enum_type,
            case_name: case_name.clone(),
            case_index: *case_index,
        },
        PatKind::Range {
            start,
            end,
            inclusive,
            is_unsigned,
        } => PatKind::Range {
            start: *start,
            end: *end,
            inclusive: *inclusive,
            is_unsigned: *is_unsigned,
        },
    };
    caller.pats.push(PatNode { kind, span })
}

/// Splice an operand from the callee into the caller. An effectful subtree is
/// spliced as an expr; a promoted pure value is re-interned into the caller's
/// pool — `ValueId`s are pool-scoped, so the whole value tree must be
/// re-allocated against the caller's pool with its child `ValueId`s (and
/// `Opaque` source locals) remapped into the caller frame.
fn splice_operand(caller: &mut Body, callee: &Body, op: Operand, ctx: &InlineCtx) -> Operand {
    match op {
        Operand::Expr(e) => Operand::Expr(splice_expr(caller, callee, e, ctx)),
        Operand::Value(v) => Operand::Value(splice_value(caller, callee, v, ctx)),
    }
}

/// Re-allocate a callee pure value (and its whole tree) into the caller's pool.
/// `ValueId`s are pool-scoped, so every child id is recursively re-allocated and
/// an `Opaque`'s source local is remapped into the caller frame — otherwise a
/// composite value (`Binary` / `Cast` / `FieldAccess` / …) would carry child ids
/// that denote unrelated values (often a different width) in the caller's pool.
fn splice_value(caller: &mut Body, callee: &Body, v: ValueId, ctx: &InlineCtx) -> ValueId {
    use crate::nir_value_graph::{OpaqueSource, ValueKind};
    let recorded_ty = callee.values.type_of(v);
    let new_kind = match callee.values.kind(v).clone() {
        ValueKind::Binary { op, lhs, rhs, ty } => ValueKind::Binary {
            op,
            lhs: splice_value(caller, callee, lhs, ctx),
            rhs: splice_value(caller, callee, rhs, ctx),
            ty,
        },
        ValueKind::Unary { op, operand, ty } => ValueKind::Unary {
            op,
            operand: splice_value(caller, callee, operand, ctx),
            ty,
        },
        ValueKind::Cast { operand, target } => ValueKind::Cast {
            operand: splice_value(caller, callee, operand, ctx),
            target,
        },
        ValueKind::Select { cond, then, else_ } => ValueKind::Select {
            cond: splice_value(caller, callee, cond, ctx),
            then: splice_value(caller, callee, then, ctx),
            else_: splice_value(caller, callee, else_, ctx),
        },
        ValueKind::LoopPhi { entry, body_iter } => ValueKind::LoopPhi {
            entry: splice_value(caller, callee, entry, ctx),
            body_iter: splice_value(caller, callee, body_iter, ctx),
        },
        ValueKind::FieldAccess {
            receiver,
            field_index,
            heap_ver,
        } => ValueKind::FieldAccess {
            receiver: splice_value(caller, callee, receiver, ctx),
            field_index,
            heap_ver,
        },
        ValueKind::Opaque(oid) => {
            // Mint a fresh caller opaque, remapping its source local into the
            // caller frame (a skeleton-`Expr` source splices that expr).
            let new = match callee.values.opaque_source(oid) {
                Some(OpaqueSource::Local(idx)) => caller
                    .values
                    .fresh_opaque_with_source(OpaqueSource::Local(ctx.local(idx))),
                Some(OpaqueSource::Expr(e)) => {
                    let spliced = splice_expr(caller, callee, e, ctx);
                    caller
                        .values
                        .fresh_opaque_with_source(OpaqueSource::Expr(spliced))
                }
                None => caller.values.fresh_opaque(),
            };
            if let Some(t) = recorded_ty {
                caller.values.set_type(new, t);
            }
            return new;
        }
        leaf => leaf,
    };
    match recorded_ty {
        Some(t) => caller.values.alloc_unshared(new_kind, t),
        None => caller.values.intern(new_kind),
    }
}

fn splice_expr(caller: &mut Body, callee: &Body, id: ExprId, ctx: &InlineCtx) -> ExprId {
    let span = callee.exprs[id].span;
    let type_id = callee.exprs[id].type_id;
    let kind = match &callee.exprs[id].kind {
        ExprKind::Local { index, name } => ExprKind::Local {
            index: ctx.local(*index),
            name: name.clone(),
        },
        ExprKind::GlobalVarSet {
            module_source,
            name,
            value,
        } => {
            let (ms, n, v) = (module_source.clone(), name.clone(), *value);
            ExprKind::GlobalVarSet {
                module_source: ms,
                name: n,
                value: splice_operand(caller, callee, v, ctx),
            }
        }
        ExprKind::Binary { left, op, right } => {
            let (l, o, r) = (*left, *op, *right);
            ExprKind::Binary {
                left: splice_operand(caller, callee, l, ctx),
                op: o,
                right: splice_operand(caller, callee, r, ctx),
            }
        }
        ExprKind::Unary { op, expr } => {
            let (o, e) = (*op, *expr);
            ExprKind::Unary {
                op: o,
                expr: splice_operand(caller, callee, e, ctx),
            }
        }
        ExprKind::Assign { target, value } => {
            let (t, v) = (*target, *value);
            ExprKind::Assign {
                target: splice_expr(caller, callee, t, ctx),
                value: splice_operand(caller, callee, v, ctx),
            }
        }
        ExprKind::Cast { expr, target_type } => {
            let (e, tt) = (*expr, *target_type);
            ExprKind::Cast {
                expr: splice_operand(caller, callee, e, ctx),
                target_type: tt,
            }
        }
        ExprKind::Call {
            func_id,
            type_args,
            args,
        } => ExprKind::Call {
            func_id: *func_id,
            type_args: type_args.clone(),
            args: args.rebuild(
                args.iter()
                    .map(|a| ArenaCallArg {
                        expr: splice_operand(caller, callee, a.expr, ctx),
                        is_mut: a.is_mut,
                    })
                    .collect(),
            ),
        },
        ExprKind::CmRawCall { target, args } => {
            let (target, args) = (target.clone(), args.clone());
            ExprKind::CmRawCall {
                target,
                args: args
                    .into_iter()
                    .map(|a| splice_operand(caller, callee, a, ctx))
                    .collect(),
            }
        }
        ExprKind::FieldAccess {
            expr,
            field_index,
            field_name,
        } => {
            let (e, fi, fname) = (*expr, *field_index, field_name.clone());
            ExprKind::FieldAccess {
                expr: splice_operand(caller, callee, e, ctx),
                field_index: fi,
                field_name: fname,
            }
        }
        ExprKind::Index { expr, index } => {
            let (e, i) = (*expr, *index);
            ExprKind::Index {
                expr: splice_operand(caller, callee, e, ctx),
                index: splice_operand(caller, callee, i, ctx),
            }
        }
        ExprKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            let (c, t, e) = (*condition, *then_branch, *else_branch);
            ExprKind::If {
                condition: splice_operand(caller, callee, c, ctx),
                then_branch: splice_block(caller, callee, t, ctx),
                else_branch: e.map(|b| splice_block(caller, callee, b, ctx)),
            }
        }
        ExprKind::Match { expr, arms } => {
            let e = *expr;
            let arms = arms.clone();
            ExprKind::Match {
                expr: splice_operand(caller, callee, e, ctx),
                arms: arms
                    .into_iter()
                    .map(|a| ArmData {
                        pattern: splice_pat(caller, callee, a.pattern, ctx),
                        guard: a.guard.map(|g| splice_operand(caller, callee, g, ctx)),
                        body: splice_operand(caller, callee, a.body, ctx),
                        span: a.span,
                    })
                    .collect(),
            }
        }
        ExprKind::StructLiteral {
            struct_type,
            struct_name,
            fields,
        } => {
            let (st, sn) = (*struct_type, struct_name.clone());
            let field_data: Vec<(String, Operand, u32)> = fields
                .iter()
                .map(|f| (f.name.clone(), f.value, f.field_index))
                .collect();
            ExprKind::StructLiteral {
                struct_type: st,
                struct_name: sn,
                fields: field_data
                    .into_iter()
                    .map(|(name, value, field_index)| ArenaStructField {
                        name,
                        value: splice_operand(caller, callee, value, ctx),
                        field_index,
                    })
                    .collect(),
            }
        }
        ExprKind::TupleLiteral { elements } => {
            let elements = elements.clone();
            ExprKind::TupleLiteral {
                elements: elements
                    .into_iter()
                    .map(|e| splice_operand(caller, callee, e, ctx))
                    .collect(),
            }
        }
        ExprKind::ArrayLiteral { elements } => {
            let elements = elements.clone();
            ExprKind::ArrayLiteral {
                elements: elements
                    .into_iter()
                    .map(|e| splice_operand(caller, callee, e, ctx))
                    .collect(),
            }
        }
        ExprKind::IndirectCall { callee: c, args } => {
            let (c, args) = (*c, args.clone());
            ExprKind::IndirectCall {
                callee: splice_operand(caller, callee, c, ctx),
                args: args
                    .into_iter()
                    .map(|a| splice_operand(caller, callee, a, ctx))
                    .collect(),
            }
        }
        ExprKind::ClosureToCanonical {
            functor,
            functor_id,
            target_fn_type,
            closure_module,
        } => {
            let (f, fid, tft, cm) = (
                *functor,
                *functor_id,
                *target_fn_type,
                closure_module.clone(),
            );
            ExprKind::ClosureToCanonical {
                functor: splice_operand(caller, callee, f, ctx),
                functor_id: fid,
                target_fn_type: tft,
                closure_module: cm,
            }
        }
        ExprKind::VariantConstruct {
            variant_type,
            case_index,
            case_name,
            payload,
        } => {
            let (vt, ci, cn, p) = (*variant_type, *case_index, case_name.clone(), *payload);
            ExprKind::VariantConstruct {
                variant_type: vt,
                case_index: ci,
                case_name: cn,
                payload: p.map(|x| splice_operand(caller, callee, x, ctx)),
            }
        }
        ExprKind::EnumConstruct {
            enum_type,
            case_index,
            case_name,
        } => ExprKind::EnumConstruct {
            enum_type: *enum_type,
            case_index: *case_index,
            case_name: case_name.clone(),
        },
        ExprKind::LabeledBlock {
            label,
            block,
            result_type,
            role,
        } => {
            let (l, b, rt, role) = (label.clone(), *block, *result_type, *role);
            ExprKind::LabeledBlock {
                label: ctx.lbl(&l),
                block: splice_block(caller, callee, b, ctx),
                result_type: rt,
                role,
            }
        }
        ExprKind::VariantTag { expr } => ExprKind::VariantTag {
            expr: splice_operand(caller, callee, *expr, ctx),
        },
        ExprKind::VariantTest {
            expr,
            case_index,
            case_name,
        } => {
            let (e, ci, cn) = (*expr, *case_index, case_name.clone());
            ExprKind::VariantTest {
                expr: splice_operand(caller, callee, e, ctx),
                case_index: ci,
                case_name: cn,
            }
        }
        ExprKind::VariantPayload {
            expr,
            case_index,
            payload_type,
        } => {
            let (e, ci, pt) = (*expr, *case_index, *payload_type);
            ExprKind::VariantPayload {
                expr: splice_operand(caller, callee, e, ctx),
                case_index: ci,
                payload_type: pt,
            }
        }
        ExprKind::Switch {
            scrutinee,
            min_value,
            table,
            arms,
            default,
        } => {
            let (s, mv, table, arms, d) = (
                *scrutinee,
                *min_value,
                table.clone(),
                arms.clone(),
                *default,
            );
            ExprKind::Switch {
                scrutinee: splice_operand(caller, callee, s, ctx),
                min_value: mv,
                table,
                arms: arms
                    .into_iter()
                    .map(|b| splice_block(caller, callee, b, ctx))
                    .collect(),
                default: splice_block(caller, callee, d, ctx),
            }
        }
        ExprKind::PackedArray(b) => ExprKind::PackedArray(b.clone()),
        ExprKind::Dead => ExprKind::Dead,
        ExprKind::GlobalVarGet {
            module_source,
            name,
        } => ExprKind::GlobalVarGet {
            module_source: module_source.clone(),
            name: name.clone(),
        },
    };
    caller.exprs.push(ExprNode {
        kind,
        type_id,
        span,
    })
}

/// Recursively inline calls within an expression
fn inline_calls_in_expr(
    body: &mut Body,
    e: ExprId,
    candidates: Candidates<'_>,
    descriptors: &[FunctionRef],
    frame: &mut CallerFrame,
    type_table: &TypeTable,
    inlined_funcs: &mut Vec<FuncId>,
    labels: &mut InlineLabels,
    reval: &mut Vec<InlineRevalInfo>,
    site: Site,
) {
    let args: Option<Vec<Operand>> = match &body.exprs[e].kind {
        ExprKind::Call { args, .. } => Some(args.iter().map(|a| a.expr).collect()),
        _ => None,
    };
    let Some(args) = args else {
        let (exprs, blocks) = inline_expr_children(body, e);
        for ex in exprs {
            inline_calls_in_expr(
                body,
                ex,
                candidates,
                descriptors,
                frame,
                type_table,
                inlined_funcs,
                labels,
                reval,
                site,
            );
        }
        for b in blocks {
            inline_calls_in_block(
                body,
                b,
                candidates,
                descriptors,
                frame,
                type_table,
                inlined_funcs,
                labels,
                reval,
                site,
            );
        }
        return;
    };

    // Recurse into arguments first, then attempt to inline this call.
    for a in args {
        let Some(a) = a.as_expr() else { continue };
        inline_calls_in_expr(
            body,
            a,
            candidates,
            descriptors,
            frame,
            type_table,
            inlined_funcs,
            labels,
            reval,
            site,
        );
    }
    if let Some((new_id, inlined_key)) =
        try_inline_call_expr(body, e, candidates, frame, type_table, labels, reval, site)
    {
        if !inlined_funcs.contains(&inlined_key) {
            inlined_funcs.push(inlined_key);
        }
        // Move the inlined labeled-block node into the call slot and null out
        // the now-dead `new_id`, so the inner block is owned by exactly one node
        // (`e`). Cloning would leave `new_id` as an orphan sharing the same
        // `BlockId`, violating the arena's one-parent-per-node invariant.
        let span = body.exprs[new_id].span;
        let moved = std::mem::replace(
            &mut body.exprs[new_id],
            ExprNode {
                kind: ExprKind::Dead,
                type_id: TypeTable::UNIT,
                span,
            },
        );
        body.exprs[e] = moved;
    }
}

#[cfg(test)]
mod cast_cost_tests {
    use super::cast_emits_instruction;
    use crate::tir::{TypeId, TypeTable};

    fn emits(from: TypeId, to: TypeId) -> bool {
        cast_emits_instruction(&TypeTable::new(), from, to)
    }

    #[test]
    fn a_cast_within_one_wasm_shape_emits_nothing() {
        // The chain `char::eq_ignore_ascii_case` folds in: all i32, all free.
        assert!(!emits(TypeTable::CHAR, TypeTable::U32));
        assert!(!emits(TypeTable::U8, TypeTable::CHAR));
        assert!(!emits(TypeTable::BOOL, TypeTable::U32));
        assert!(!emits(TypeTable::U32, TypeTable::I32));
        assert!(!emits(TypeTable::I32, TypeTable::CHAR));
    }

    #[test]
    fn a_cast_across_wasm_shapes_emits_a_conversion() {
        assert!(emits(TypeTable::I32, TypeTable::I64));
        assert!(emits(TypeTable::I64, TypeTable::I32));
        assert!(emits(TypeTable::U32, TypeTable::F64));
        assert!(emits(TypeTable::F64, TypeTable::I32));
        assert!(emits(TypeTable::F32, TypeTable::F64));
    }

    #[test]
    fn a_narrowing_below_i32_emits_its_mask() {
        // `truncate_to_sub_i32` masks or sign-extends these even though the
        // source already sits in an i32.
        assert!(emits(TypeTable::I32, TypeTable::U8));
        assert!(emits(TypeTable::I32, TypeTable::I8));
        assert!(emits(TypeTable::CHAR, TypeTable::U16));
        assert!(emits(TypeTable::U32, TypeTable::I16));
    }

    #[test]
    fn a_shape_the_walk_cannot_read_stays_charged() {
        // Narrowing the estimate is only safe where the shape is known; `v128`
        // and the unit type are not scalars this classifies.
        assert!(emits(TypeTable::V128, TypeTable::V128));
        assert!(emits(TypeTable::UNIT, TypeTable::I32));
    }
}
