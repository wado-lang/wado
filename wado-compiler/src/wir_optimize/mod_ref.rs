//! Per-instruction mod/ref summary for WIR.
//!
//! A [`ModRef`] is a coarse, conservative summary of what a
//! [`WirInstr`] (together with its sub-tree) does to machine state:
//! which locals/globals it reads and writes, whether it touches the GC
//! heap or linear memory, whether it may transfer control non-locally,
//! whether it can call into arbitrary user code, and whether it may
//! trap.
//!
//! Unrelated to Wado's algebraic-effect / `with`-clause machinery
//! (`effect_check.rs`): this is the classical compiler-optimization
//! notion (cf. LLVM `ModRefInfo`, GCC `mod` / `ref` sets).
//!
//! ## Client API
//!
//! Passes consume the summary through three predicates:
//!
//! - [`ModRef::is_re_evaluation_safe`] — can an expression with this
//!   summary be moved to a later program point without changing
//!   observable behavior? (Used by pure-elision passes.)
//! - [`ModRef::may_clobber`] — could the writes implied by `self`
//!   invalidate any read implied by `other`? (Used by motion /
//!   redundant-store passes.)
//! - [`can_move_past`] — convenience for the common "skip an
//!   intervening statement while erasing a candidate local" check.
//!
//! ## Granularity
//!
//! Coarse for v1: heap and linear memory each carry a single
//! read/write flag pair, calls are "everything," and traps are a
//! single boolean. Passes never inspect the internal representation
//! directly — they only call the predicates above — so refining the
//! representation later (per-`WirTypeId` GC heap, per-field reads,
//! per-call effect signatures, …) does not require call-site churn.
//!
//! ## Discipline for extending [`WirInstr`]
//!
//! [`ModRef::accumulate`] enumerates every effectful variant
//! explicitly. Pure value-producing variants (constants, arithmetic,
//! comparisons, SIMD lane ops, etc.) fall into a single catch-all arm
//! that just merges children via [`WirInstr::for_each_child`]. When
//! adding a new [`WirInstr`] variant that introduces a new kind of
//! effect, add it explicitly to `accumulate` — otherwise it silently
//! defaults to "pure value op" and the soundness of downstream passes
//! is lost.
//!
//! The companion test [`tests::known_effectful_variants_are_explicit`]
//! constructs each kind of effectful instruction and asserts that the
//! summary picks up the expected flag, so an accidental fall-through
//! into the catch-all surfaces as a test failure.

use crate::hashmap::IndexSet;
use crate::wir::WirInstr;

/// Read / write flags for a single state channel (e.g., GC heap or
/// linear memory).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Channel {
    pub reads: bool,
    pub writes: bool,
}

impl Channel {
    #[allow(dead_code)] // used by ModRef::join (consumed by future D2)
    fn join(&mut self, other: Channel) {
        self.reads |= other.reads;
        self.writes |= other.writes;
    }
}

/// Control-flow effect of an instruction tree.
///
/// Ordered weakest → strongest; merging takes the [`Ord`] max.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Control {
    /// Completes normally and returns the value of the last
    /// sub-expression. Covers pure value ops, GC reads, calls, and
    /// `LocalSet` / `GlobalSet` / `StructSet` / `ArraySet` (which are
    /// observable as writes but transfer control linearly).
    #[default]
    Linear,
    /// Body is executed only on some inputs (`If`) or repeatedly
    /// (`Loop`). Effects are still tracked as the union of all paths;
    /// the `Conditional` value only signals to motion passes that the
    /// path through this instruction is not unique.
    Conditional,
    /// Transfers control out of the enclosing scope (`Br`, `BrIf`,
    /// `BrTable`, `Return`, `Unreachable`). Once an intervening
    /// statement carries this label, subsequent siblings — including
    /// any use we hoped to substitute into — may never execute.
    NonLocal,
}

impl Control {
    fn join(&mut self, other: Control) {
        if other > *self {
            *self = other;
        }
    }
}

/// Modifies / references summary of a [`WirInstr`] and its sub-tree.
///
/// All fields are conservatively monotonic: once a flag is `true` or a
/// set is non-empty, refining the summary only adds more information
/// (a finer-grained `Channel` representation, per-callee effect
/// summaries, …) — it never invalidates a pass that relied on the
/// coarse signal.
#[derive(Debug, Clone, Default)]
pub(super) struct ModRef {
    /// Locals named by any `LocalGet` in the tree.
    pub local_reads: IndexSet<String>,
    /// Locals named by any `LocalSet`, `LocalTee`, or
    /// `MultiValueLocalBind` slot.
    pub local_writes: IndexSet<String>,
    /// Globals (by `WirName::fq`) named by any `GlobalGet`.
    ///
    /// `WirName` itself does not derive `Eq` / `Hash`, so we key on
    /// its fully-qualified string. Equivalent under module-qualified
    /// identity.
    pub global_reads: IndexSet<String>,
    /// Globals (by `WirName::fq`) named by any `GlobalSet`.
    pub global_writes: IndexSet<String>,
    /// GC heap: structs, arrays, and tables (table entries are GC
    /// references managed by the same collector). v1 is coarse: a
    /// single read / write pair, no per-type or per-field refinement.
    pub heap: Channel,
    /// Linear memory (`memory.*` loads / stores, `memory.size`,
    /// `memory.grow`).
    pub memory: Channel,
    /// Control transfer behavior of the tree.
    pub control: Control,
    /// Tree contains a `Call`, `CallIndirect`, `CallRef`, or
    /// `ArrayClone` with an element-copy helper. Callees may touch
    /// arbitrary state we cannot see; treat as "everything" until we
    /// thread per-callee effect summaries through.
    pub calls: bool,
    /// Tree allocates one or more fresh GC objects or arrays. Distinct
    /// from `heap.writes`: allocation does **not** clobber existing
    /// heap state, but it produces a new object identity (observable
    /// via `ref.eq`) and so prevents re-evaluation at a different
    /// program point.
    pub allocates: bool,
    /// Tree may trap at runtime (`ref.as_non_null` on null, integer
    /// divide / remainder by zero, OOB array index, OOB memory load /
    /// store, …). Independent of [`Control::NonLocal`]: a trap halts
    /// execution rather than branching to a sibling.
    pub may_trap: bool,
}

impl ModRef {
    /// Compute the summary of `instr` and its sub-tree.
    pub fn of(instr: &WirInstr) -> Self {
        let mut mr = ModRef::default();
        mr.accumulate(instr);
        mr
    }

    /// Merge another summary into `self`. Sets union, flags OR,
    /// control takes [`Ord`] max.
    ///
    /// Exposed for use by passes that build a summary out of multiple
    /// disjoint sub-trees (e.g. D2's redundant-store elimination
    /// folding a candidate `StructNew`'s field initializers with an
    /// explicit "writes the candidate local" entry).
    #[allow(dead_code)]
    pub fn join(&mut self, other: ModRef) {
        self.local_reads.extend(other.local_reads);
        self.local_writes.extend(other.local_writes);
        self.global_reads.extend(other.global_reads);
        self.global_writes.extend(other.global_writes);
        self.heap.join(other.heap);
        self.memory.join(other.memory);
        self.control.join(other.control);
        self.calls |= other.calls;
        self.allocates |= other.allocates;
        self.may_trap |= other.may_trap;
    }

    /// True iff re-evaluating an expression with this summary at a
    /// later program point cannot change observable behavior.
    ///
    /// Rejects calls, allocations (would create a new identity), any
    /// heap or memory access (state may be mutated between the
    /// original position and the new one), any write (would re-fire
    /// at the new position, causing double effects), and any non-local
    /// control transfer. Trapping ops are NOT rejected — re-evaluation
    /// preserves the trap condition; the trap point shifts but the
    /// halt is the same observable event.
    ///
    /// Exposed for the planned migration of `is_pure_for_elision` in
    /// [`elide_single_field_struct_locals`] to consult this summary
    /// instead of its bespoke variant whitelist.
    #[allow(dead_code)]
    pub fn is_re_evaluation_safe(&self) -> bool {
        !self.calls
            && !self.allocates
            && !self.heap.reads
            && !self.heap.writes
            && !self.memory.reads
            && !self.memory.writes
            && self.local_writes.is_empty()
            && self.global_writes.is_empty()
            && matches!(self.control, Control::Linear)
    }

    /// True iff `self`'s writes (or any call inside `self`) might
    /// invalidate any of `other`'s reads.
    ///
    /// Call effects: callees can mutate globals, the GC heap, and
    /// linear memory, so a call clobbers reads of those channels.
    /// Callees CANNOT reach the caller's Wasm locals — locals live in
    /// the calling frame and are not addressable from another
    /// function — so a call does not clobber a `local_reads`-only
    /// expression. Refining this further (per-callee global/heap
    /// effect summaries) would only narrow the global/heap branches.
    pub fn may_clobber(&self, other: &ModRef) -> bool {
        if self.calls
            && (!other.global_reads.is_empty() || other.heap.reads || other.memory.reads)
        {
            return true;
        }
        if !self.local_writes.is_disjoint(&other.local_reads) {
            return true;
        }
        if !self.global_writes.is_disjoint(&other.global_reads) {
            return true;
        }
        if self.heap.writes && other.heap.reads {
            return true;
        }
        if self.memory.writes && other.memory.reads {
            return true;
        }
        false
    }

    /// Walk `instr` recursively and fold its effects into `self`.
    ///
    /// Effectful variants are handled explicitly; pure value ops fall
    /// into the catch-all arm at the bottom and only merge children.
    /// See the module doc for the extension discipline.
    fn accumulate(&mut self, instr: &WirInstr) {
        use WirInstr::{
            ArrayClone, ArrayCopy, ArrayFill, ArrayGet, ArrayGetS, ArrayGetU, ArrayLen, ArrayNew,
            ArrayNewData, ArrayNewDefault, ArrayNewFixed, ArraySet, Block, Br, BrIf, BrTable, Call,
            CallIndirect, CallRef, GlobalGet, GlobalSet, I32DivS, I32DivU, I32Load, I32Load8S,
            I32Load8U, I32Load16S, I32Load16U, I32RemS, I32RemU, I32Store, I32Store8, I32Store16,
            I32TruncF32S, I32TruncF32U, I32TruncF64S, I32TruncF64U, I64DivS, I64DivU, I64Load,
            I64RemS, I64RemU, I64Store, I64TruncF32S, I64TruncF32U, I64TruncF64S, I64TruncF64U, If,
            LocalGet, LocalSet, LocalTee, Loop, MemoryFill, MemoryGrow, MemorySize,
            MultiValueLocalBind, RefAsNonNull, RefCast, Return, Seq, StructGet, StructNew,
            StructSet, TableGet, TableSet, Unreachable, V128Load, V128Store,
        };
        match instr {
            // === Locals ===
            LocalGet { name, .. } => {
                self.local_reads.insert(name.clone());
            }
            LocalSet { name, value } | LocalTee { name, value } => {
                self.local_writes.insert(name.clone());
                self.accumulate(value);
            }

            // === Globals ===
            GlobalGet { name, .. } => {
                self.global_reads.insert(name.fq.clone());
            }
            GlobalSet { name, value } => {
                self.global_writes.insert(name.fq.clone());
                self.accumulate(value);
            }

            // === GC heap reads ===
            StructGet { expr, .. } => {
                self.heap.reads = true;
                self.may_trap = true; // null receiver
                self.accumulate(expr);
            }
            ArrayGet { array, index, .. }
            | ArrayGetS { array, index, .. }
            | ArrayGetU { array, index, .. } => {
                self.heap.reads = true;
                self.may_trap = true; // null + OOB
                self.accumulate(array);
                self.accumulate(index);
            }
            ArrayLen(arr) => {
                self.heap.reads = true;
                self.may_trap = true; // null receiver
                self.accumulate(arr);
            }

            // === GC heap writes ===
            StructSet { expr, value, .. } => {
                self.heap.writes = true;
                self.may_trap = true; // null receiver
                self.accumulate(expr);
                self.accumulate(value);
            }
            ArraySet {
                array,
                index,
                value,
                ..
            } => {
                self.heap.writes = true;
                self.may_trap = true; // null + OOB
                self.accumulate(array);
                self.accumulate(index);
                self.accumulate(value);
            }
            ArrayCopy {
                dest,
                dest_offset,
                src,
                src_offset,
                len,
                ..
            } => {
                self.heap.reads = true;
                self.heap.writes = true;
                self.may_trap = true; // null + OOB on either side
                self.accumulate(dest);
                self.accumulate(dest_offset);
                self.accumulate(src);
                self.accumulate(src_offset);
                self.accumulate(len);
            }
            ArrayFill {
                array,
                offset,
                value,
                len,
                ..
            } => {
                self.heap.writes = true;
                self.may_trap = true; // null + OOB
                self.accumulate(array);
                self.accumulate(offset);
                self.accumulate(value);
                self.accumulate(len);
            }

            // === GC heap allocations ===
            StructNew { fields, .. } => {
                self.allocates = true;
                for field in fields {
                    self.accumulate(field);
                }
            }
            ArrayNew { init, len, .. } => {
                self.allocates = true;
                self.accumulate(init);
                self.accumulate(len);
            }
            ArrayNewDefault { len, .. } => {
                self.allocates = true;
                self.accumulate(len);
            }
            ArrayNewData { offset, len, .. } => {
                self.allocates = true;
                self.may_trap = true; // offset + len overruns the data segment
                self.accumulate(offset);
                self.accumulate(len);
            }
            ArrayNewFixed { elements, .. } => {
                self.allocates = true;
                for e in elements {
                    self.accumulate(e);
                }
            }
            ArrayClone {
                src,
                element_copy_func,
                ..
            } => {
                self.allocates = true;
                self.heap.reads = true; // deep-reads src elements
                self.may_trap = true; // null src
                if element_copy_func.is_some() {
                    // The named helper is called once per element and
                    // can do anything.
                    self.calls = true;
                }
                self.accumulate(src);
            }

            // === Reference ops with traps ===
            RefAsNonNull(inner) => {
                self.may_trap = true;
                self.accumulate(inner);
            }
            RefCast { expr, .. } => {
                self.may_trap = true;
                self.accumulate(expr);
            }

            // === Tables ===
            TableGet { index, .. } => {
                self.heap.reads = true;
                self.may_trap = true; // OOB
                self.accumulate(index);
            }
            TableSet { index, value, .. } => {
                self.heap.writes = true;
                self.may_trap = true; // OOB
                self.accumulate(index);
                self.accumulate(value);
            }

            // === Calls ===
            Call { args, .. } => {
                self.calls = true;
                for a in args {
                    self.accumulate(a);
                }
            }
            CallIndirect { args, index, .. } => {
                self.calls = true;
                for a in args {
                    self.accumulate(a);
                }
                self.accumulate(index);
            }
            CallRef { args, func_ref, .. } => {
                self.calls = true;
                for a in args {
                    self.accumulate(a);
                }
                self.accumulate(func_ref);
            }

            // === Linear memory ===
            MemorySize => {
                // memory.size returns metadata that `memory.grow`
                // mutates; treat as a memory read.
                self.memory.reads = true;
            }
            MemoryGrow(arg) => {
                self.memory.writes = true;
                self.accumulate(arg);
            }
            MemoryFill { dst, value, len } => {
                self.memory.writes = true;
                self.may_trap = true; // OOB
                self.accumulate(dst);
                self.accumulate(value);
                self.accumulate(len);
            }
            I32Load { addr, .. }
            | I32Load8U { addr, .. }
            | I32Load8S { addr, .. }
            | I32Load16U { addr, .. }
            | I32Load16S { addr, .. }
            | I64Load { addr, .. }
            | V128Load { addr, .. } => {
                self.memory.reads = true;
                self.may_trap = true; // OOB
                self.accumulate(addr);
            }
            I32Store { addr, value, .. }
            | I32Store8 { addr, value, .. }
            | I32Store16 { addr, value, .. }
            | I64Store { addr, value, .. }
            | V128Store { addr, value, .. } => {
                self.memory.writes = true;
                self.may_trap = true; // OOB
                self.accumulate(addr);
                self.accumulate(value);
            }

            // === Integer divide / remainder (trap on zero divisor;
            // signed div / rem of MIN by -1 overflows) ===
            I32DivS(a, b)
            | I32DivU(a, b)
            | I32RemS(a, b)
            | I32RemU(a, b)
            | I64DivS(a, b)
            | I64DivU(a, b)
            | I64RemS(a, b)
            | I64RemU(a, b) => {
                self.may_trap = true;
                self.accumulate(a);
                self.accumulate(b);
            }

            // === Non-saturating float→int truncation (traps on
            // out-of-range / NaN; the `TruncSat*` variants do not) ===
            I32TruncF32S(o) | I32TruncF32U(o) | I32TruncF64S(o) | I32TruncF64U(o)
            | I64TruncF32S(o) | I64TruncF32U(o) | I64TruncF64S(o) | I64TruncF64U(o) => {
                self.may_trap = true;
                self.accumulate(o);
            }

            // === Control transfer ===
            Br { .. } => {
                self.control.join(Control::NonLocal);
            }
            BrIf { condition, .. } => {
                self.control.join(Control::NonLocal);
                self.accumulate(condition);
            }
            BrTable { index, .. } => {
                self.control.join(Control::NonLocal);
                self.accumulate(index);
            }
            Return { value } => {
                self.control.join(Control::NonLocal);
                if let Some(v) = value {
                    self.accumulate(v);
                }
            }
            Unreachable => {
                self.control.join(Control::NonLocal);
            }

            // === Conditional / iterative control structures ===
            //
            // `If` / `Loop` propagate their body's effects but also
            // raise the parent's `control` to `Conditional` — unless
            // the body already raised it to `NonLocal`, which
            // dominates.
            //
            // Over-approximation: a `Br { depth: 0 }` inside a
            // `Loop` stays within the loop (it iterates), but we
            // surface it as `NonLocal` via the `Br` arm above. A
            // future refinement could track scope depth to avoid this
            // over-approximation; D1 does not rely on Loop / If
            // intervening statements being legal to skip, so the
            // current shape is sound.
            If {
                condition,
                then_body,
                else_body,
                ..
            } => {
                self.accumulate(condition);
                for s in then_body {
                    self.accumulate(s);
                }
                if let Some(eb) = else_body {
                    for s in eb {
                        self.accumulate(s);
                    }
                }
                if self.control < Control::NonLocal {
                    self.control = Control::Conditional;
                }
            }
            Loop { body, .. } => {
                for s in body {
                    self.accumulate(s);
                }
                if self.control < Control::NonLocal {
                    self.control = Control::Conditional;
                }
            }
            Block { body, .. } | Seq(body) => {
                // A `Block` itself does not introduce conditionality
                // — its body executes in order. Any non-local exit
                // inside the body propagates up via the `Br` /
                // `Return` / `Unreachable` arms.
                for s in body {
                    self.accumulate(s);
                }
            }

            // === Multi-value local bind ===
            //
            // Executes the inner instruction, then binds the N
            // top-of-stack values to the named locals (entry `None`
            // drops the value).
            MultiValueLocalBind { instr, locals } => {
                self.accumulate(instr);
                for name in locals.iter().flatten() {
                    self.local_writes.insert(name.clone());
                }
            }

            // === Pure value-producing ops (catch-all) ===
            //
            // Everything else — constants, arithmetic, comparisons,
            // SIMD lane ops, ref tests, `Drop`, `Select`,
            // `BranchHint`, `Nop`, `DeclareLocal`, `RefFunc`,
            // `RefNull`, `RefIsNull`, `RefEq`, `RefI31`, integer
            // extensions, reinterprets, saturating truncations, and
            // so on — contributes nothing of its own and just merges
            // its children's effects. See the module doc for the
            // discipline when adding new variants.
            _ => {
                instr.for_each_child(&mut |child| self.accumulate(child));
            }
        }
    }
}

/// Can the expression with summary `expr_mr` be moved past an
/// intervening statement with summary `int_mr`, while a candidate
/// local `candidate_name` is being eliminated by the rewrite?
///
/// Soundness conditions (all must hold):
///
/// 1. The intervening statement transfers control linearly
///    (`control == Linear`). `Conditional` and `NonLocal` intervenings
///    are rejected: in the `NonLocal` case the use site may never
///    execute; in the `Conditional` case some path through the
///    intervening might still escape via an inner `Br`, and we
///    over-approximate by bailing.
/// 2. The intervening statement does not read the candidate local.
///    With single-use candidates this is normally already guaranteed
///    by the pass's stats check; the test is a cheap defense against
///    future refactors that loosen the single-use precondition.
/// 3. The intervening and the expression do not both `may_trap`.
///    If only one of them traps, the final program behavior is the
///    same in either order (the surviving trap fires at its own
///    point); if both can trap, the order of traps changes and
///    observable trap location differs. Conservative for v1.
/// 4. The intervening statement's writes (or any call inside it) do
///    not clobber any of the expression's reads (the may-alias core
///    of [`ModRef::may_clobber`]).
pub(super) fn can_move_past(expr_mr: &ModRef, int_mr: &ModRef, candidate_name: &str) -> bool {
    if !matches!(int_mr.control, Control::Linear) {
        return false;
    }
    if int_mr.local_reads.contains(candidate_name) {
        return false;
    }
    if int_mr.may_trap && expr_mr.may_trap {
        return false;
    }
    !int_mr.may_clobber(expr_mr)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wir::{WirInstr, WirName, WirType, WirTypeId};
    use std::rc::Rc;

    fn local_get(name: &str) -> WirInstr {
        WirInstr::LocalGet {
            name: name.to_string(),
            result_ty: WirType::I32,
        }
    }

    fn local_set(name: &str, value: WirInstr) -> WirInstr {
        WirInstr::LocalSet {
            name: name.to_string(),
            value: Box::new(value),
        }
    }

    fn global_name(fq: &str) -> WirName {
        WirName { fq: fq.to_string() }
    }

    fn type_id() -> WirTypeId {
        WirTypeId::new(0, Rc::from("test:T"))
    }

    fn struct_new(fields: Vec<WirInstr>) -> WirInstr {
        WirInstr::StructNew {
            type_id: type_id(),
            fields,
        }
    }

    fn struct_get(expr: WirInstr) -> WirInstr {
        WirInstr::StructGet {
            type_id: type_id(),
            field_name: "value".to_string(),
            expr: Box::new(expr),
            result_ty: WirType::I32,
        }
    }

    fn struct_set(expr: WirInstr, value: WirInstr) -> WirInstr {
        WirInstr::StructSet {
            type_id: type_id(),
            field_name: "value".to_string(),
            expr: Box::new(expr),
            value: Box::new(value),
        }
    }

    fn call(args: Vec<WirInstr>) -> WirInstr {
        // The exact function id is irrelevant for the ModRef test.
        WirInstr::Call {
            func_id: crate::wir::WirFuncId::new(0, Rc::from("test:f")),
            args,
        }
    }

    // -----------------------------------------------------------------
    // Leaf categorization
    // -----------------------------------------------------------------

    #[test]
    fn local_get_reads_local() {
        let mr = ModRef::of(&local_get("x"));
        assert!(mr.local_reads.contains("x"));
        assert!(mr.local_writes.is_empty());
        assert!(!mr.heap.reads);
        assert!(!mr.may_trap);
        assert_eq!(mr.control, Control::Linear);
    }

    #[test]
    fn local_set_writes_local_and_inherits_rhs_reads() {
        let mr = ModRef::of(&local_set("y", local_get("x")));
        assert!(mr.local_writes.contains("y"));
        assert!(mr.local_reads.contains("x"));
    }

    #[test]
    fn global_get_records_fq() {
        let mr = ModRef::of(&WirInstr::GlobalGet {
            name: global_name("mod::G"),
            result_ty: WirType::I32,
        });
        assert!(mr.global_reads.contains("mod::G"));
        assert!(mr.global_writes.is_empty());
    }

    #[test]
    fn global_set_records_fq_and_rhs() {
        let mr = ModRef::of(&WirInstr::GlobalSet {
            name: global_name("mod::G"),
            value: Box::new(local_get("x")),
        });
        assert!(mr.global_writes.contains("mod::G"));
        assert!(mr.local_reads.contains("x"));
    }

    // -----------------------------------------------------------------
    // Heap reads / writes / allocations
    // -----------------------------------------------------------------

    #[test]
    fn struct_get_is_heap_read_and_may_trap() {
        let mr = ModRef::of(&struct_get(local_get("r")));
        assert!(mr.heap.reads);
        assert!(!mr.heap.writes);
        assert!(mr.may_trap);
        assert!(!mr.allocates);
        assert!(mr.local_reads.contains("r"));
    }

    #[test]
    fn struct_set_is_heap_write_and_may_trap() {
        let mr = ModRef::of(&struct_set(local_get("r"), WirInstr::I32Const(0)));
        assert!(mr.heap.writes);
        assert!(!mr.heap.reads);
        assert!(mr.may_trap);
    }

    #[test]
    fn struct_new_allocates_but_does_not_write_heap() {
        let mr = ModRef::of(&struct_new(vec![local_get("x")]));
        assert!(mr.allocates);
        assert!(
            !mr.heap.writes,
            "allocation creates new identity, does not clobber existing heap"
        );
        assert!(mr.local_reads.contains("x"));
    }

    #[test]
    fn array_get_is_heap_read_and_may_trap() {
        let mr = ModRef::of(&WirInstr::ArrayGet {
            type_id: type_id(),
            array: Box::new(local_get("a")),
            index: Box::new(WirInstr::I32Const(0)),
            result_ty: WirType::I32,
        });
        assert!(mr.heap.reads);
        assert!(mr.may_trap);
    }

    #[test]
    fn array_set_is_heap_write_and_may_trap() {
        let mr = ModRef::of(&WirInstr::ArraySet {
            type_id: type_id(),
            array: Box::new(local_get("a")),
            index: Box::new(WirInstr::I32Const(0)),
            value: Box::new(WirInstr::I32Const(1)),
        });
        assert!(mr.heap.writes);
        assert!(mr.may_trap);
    }

    #[test]
    fn array_copy_reads_and_writes_heap() {
        let mr = ModRef::of(&WirInstr::ArrayCopy {
            dest_type_id: type_id(),
            src_type_id: type_id(),
            dest: Box::new(local_get("d")),
            dest_offset: Box::new(WirInstr::I32Const(0)),
            src: Box::new(local_get("s")),
            src_offset: Box::new(WirInstr::I32Const(0)),
            len: Box::new(WirInstr::I32Const(0)),
        });
        assert!(mr.heap.reads);
        assert!(mr.heap.writes);
        assert!(mr.may_trap);
    }

    #[test]
    fn array_len_is_heap_read_and_may_trap() {
        let mr = ModRef::of(&WirInstr::ArrayLen(Box::new(local_get("a"))));
        assert!(mr.heap.reads);
        assert!(mr.may_trap);
    }

    // -----------------------------------------------------------------
    // Calls
    // -----------------------------------------------------------------

    #[test]
    fn call_sets_calls_and_inherits_arg_reads() {
        let mr = ModRef::of(&call(vec![local_get("x")]));
        assert!(mr.calls);
        assert!(mr.local_reads.contains("x"));
    }

    // -----------------------------------------------------------------
    // Linear memory
    // -----------------------------------------------------------------

    #[test]
    fn i32_load_is_memory_read_and_may_trap() {
        let mr = ModRef::of(&WirInstr::I32Load {
            offset: 0,
            align: 0,
            addr: Box::new(WirInstr::I32Const(0)),
        });
        assert!(mr.memory.reads);
        assert!(!mr.memory.writes);
        assert!(mr.may_trap);
    }

    #[test]
    fn i32_store_is_memory_write_and_may_trap() {
        let mr = ModRef::of(&WirInstr::I32Store {
            offset: 0,
            align: 0,
            addr: Box::new(WirInstr::I32Const(0)),
            value: Box::new(WirInstr::I32Const(0)),
        });
        assert!(mr.memory.writes);
        assert!(mr.may_trap);
    }

    #[test]
    fn memory_size_is_memory_read() {
        let mr = ModRef::of(&WirInstr::MemorySize);
        assert!(mr.memory.reads);
        assert!(!mr.memory.writes);
    }

    #[test]
    fn memory_grow_is_memory_write() {
        let mr = ModRef::of(&WirInstr::MemoryGrow(Box::new(WirInstr::I32Const(1))));
        assert!(mr.memory.writes);
    }

    // -----------------------------------------------------------------
    // Trapping arithmetic
    // -----------------------------------------------------------------

    #[test]
    fn integer_divide_may_trap() {
        let mr = ModRef::of(&WirInstr::I32DivS(
            Box::new(WirInstr::I32Const(1)),
            Box::new(WirInstr::I32Const(0)),
        ));
        assert!(mr.may_trap);
        assert_eq!(mr.control, Control::Linear);
    }

    #[test]
    fn float_to_int_trunc_may_trap() {
        let mr = ModRef::of(&WirInstr::I32TruncF64S(Box::new(WirInstr::F64Const(0.0))));
        assert!(mr.may_trap);
    }

    #[test]
    fn integer_add_does_not_trap() {
        let mr = ModRef::of(&WirInstr::I32Add(
            Box::new(WirInstr::I32Const(1)),
            Box::new(WirInstr::I32Const(2)),
        ));
        assert!(!mr.may_trap);
    }

    // -----------------------------------------------------------------
    // Control flow
    // -----------------------------------------------------------------

    #[test]
    fn br_is_non_local() {
        let mr = ModRef::of(&WirInstr::Br { depth: 0 });
        assert_eq!(mr.control, Control::NonLocal);
    }

    #[test]
    fn return_is_non_local() {
        let mr = ModRef::of(&WirInstr::Return { value: None });
        assert_eq!(mr.control, Control::NonLocal);
    }

    #[test]
    fn unreachable_is_non_local() {
        let mr = ModRef::of(&WirInstr::Unreachable);
        assert_eq!(mr.control, Control::NonLocal);
    }

    #[test]
    fn linear_if_with_pure_arms_is_conditional() {
        let mr = ModRef::of(&WirInstr::If {
            condition: Box::new(WirInstr::I32Const(1)),
            result: None,
            then_body: vec![WirInstr::I32Const(1)],
            else_body: Some(vec![WirInstr::I32Const(2)]),
        });
        assert_eq!(mr.control, Control::Conditional);
    }

    #[test]
    fn if_with_non_local_arm_is_non_local() {
        let mr = ModRef::of(&WirInstr::If {
            condition: Box::new(WirInstr::I32Const(1)),
            result: None,
            then_body: vec![WirInstr::Br { depth: 0 }],
            else_body: None,
        });
        assert_eq!(mr.control, Control::NonLocal);
    }

    #[test]
    fn loop_with_pure_body_is_conditional() {
        let mr = ModRef::of(&WirInstr::Loop {
            label: None,
            body: vec![WirInstr::I32Const(0)],
        });
        assert_eq!(mr.control, Control::Conditional);
    }

    #[test]
    fn block_with_pure_body_is_linear() {
        let mr = ModRef::of(&WirInstr::Block {
            label: None,
            result: None,
            body: vec![local_set("x", WirInstr::I32Const(0))],
        });
        assert_eq!(mr.control, Control::Linear);
        assert!(mr.local_writes.contains("x"));
    }

    // -----------------------------------------------------------------
    // Composition
    // -----------------------------------------------------------------

    #[test]
    fn nested_call_inside_struct_new_is_propagated() {
        // struct.new T { value: f() }
        let mr = ModRef::of(&struct_new(vec![call(vec![])]));
        assert!(mr.calls);
        assert!(mr.allocates);
    }

    #[test]
    fn join_unions_sets_and_flags() {
        let a = ModRef::of(&local_set("x", local_get("a")));
        let b = ModRef::of(&local_set("y", local_get("b")));
        let mut merged = a;
        merged.join(b);
        assert!(merged.local_writes.contains("x"));
        assert!(merged.local_writes.contains("y"));
        assert!(merged.local_reads.contains("a"));
        assert!(merged.local_reads.contains("b"));
    }

    // -----------------------------------------------------------------
    // Predicates
    // -----------------------------------------------------------------

    #[test]
    fn pure_expression_is_re_evaluation_safe() {
        // 5 + a
        let mr = ModRef::of(&WirInstr::I32Add(
            Box::new(WirInstr::I32Const(5)),
            Box::new(local_get("a")),
        ));
        assert!(mr.is_re_evaluation_safe());
    }

    #[test]
    fn heap_read_is_not_re_evaluation_safe() {
        let mr = ModRef::of(&struct_get(local_get("r")));
        assert!(!mr.is_re_evaluation_safe());
    }

    #[test]
    fn call_is_not_re_evaluation_safe() {
        let mr = ModRef::of(&call(vec![]));
        assert!(!mr.is_re_evaluation_safe());
    }

    #[test]
    fn struct_new_is_not_re_evaluation_safe_due_to_allocation() {
        let mr = ModRef::of(&struct_new(vec![WirInstr::I32Const(0)]));
        assert!(!mr.is_re_evaluation_safe());
    }

    #[test]
    fn may_clobber_local_write_vs_local_read() {
        let writer = ModRef::of(&local_set("x", WirInstr::I32Const(0)));
        let reader = ModRef::of(&local_get("x"));
        assert!(writer.may_clobber(&reader));
    }

    #[test]
    fn may_clobber_unrelated_locals_is_false() {
        let writer = ModRef::of(&local_set("y", WirInstr::I32Const(0)));
        let reader = ModRef::of(&local_get("x"));
        assert!(!writer.may_clobber(&reader));
    }

    #[test]
    fn may_clobber_heap_write_vs_heap_read() {
        let writer = ModRef::of(&struct_set(local_get("r"), WirInstr::I32Const(0)));
        let reader = ModRef::of(&struct_get(local_get("q")));
        assert!(writer.may_clobber(&reader));
    }

    #[test]
    fn may_clobber_pure_local_copy_does_not_clobber_heap_read() {
        let writer = ModRef::of(&local_set("f", local_get("g")));
        let reader = ModRef::of(&struct_get(local_get("r")));
        assert!(!writer.may_clobber(&reader));
    }

    #[test]
    fn may_clobber_call_clobbers_heap_read() {
        let writer = ModRef::of(&call(vec![]));
        let reader = ModRef::of(&struct_get(local_get("r")));
        assert!(writer.may_clobber(&reader));
    }

    #[test]
    fn may_clobber_call_clobbers_global_read() {
        let writer = ModRef::of(&call(vec![]));
        let reader = ModRef::of(&WirInstr::GlobalGet {
            name: global_name("mod::G"),
            result_ty: WirType::I32,
        });
        assert!(writer.may_clobber(&reader));
    }

    #[test]
    fn may_clobber_call_does_not_clobber_local_only_read() {
        // Wasm locals are private to the calling frame; callees
        // cannot reach them, so a pure local read survives any call.
        let writer = ModRef::of(&call(vec![]));
        let reader = ModRef::of(&local_get("x"));
        assert!(!writer.may_clobber(&reader));
    }

    #[test]
    fn may_clobber_call_does_not_clobber_empty_read_set() {
        let writer = ModRef::of(&call(vec![]));
        let reader = ModRef::default();
        assert!(!writer.may_clobber(&reader));
    }

    // -----------------------------------------------------------------
    // can_move_past
    // -----------------------------------------------------------------

    #[test]
    fn can_move_past_pure_local_copy() {
        // FTS shape:
        //   expr = StructGet(LocalGet("r"), "total_bytes")
        //   intervening = LocalSet("f_17", LocalGet("__local_6"))
        let expr = ModRef::of(&struct_get(local_get("r")));
        let intervening = ModRef::of(&local_set("f_17", local_get("__local_6")));
        assert!(can_move_past(&expr, &intervening, "self_16"));
    }

    #[test]
    fn cannot_move_past_when_intervening_writes_inner_read() {
        // expr reads r; intervening writes r.
        let expr = ModRef::of(&struct_get(local_get("r")));
        let intervening = ModRef::of(&local_set("r", WirInstr::I32Const(0)));
        assert!(!can_move_past(&expr, &intervening, "self"));
    }

    #[test]
    fn cannot_move_past_heap_write_when_expr_reads_heap() {
        let expr = ModRef::of(&struct_get(local_get("r")));
        let intervening = ModRef::of(&struct_set(local_get("q"), WirInstr::I32Const(0)));
        assert!(!can_move_past(&expr, &intervening, "self"));
    }

    #[test]
    fn can_move_pure_expr_past_heap_write() {
        // Inner has no heap reads, so an intervening heap write is fine.
        let expr = ModRef::of(&WirInstr::I32Add(
            Box::new(WirInstr::I32Const(5)),
            Box::new(local_get("a")),
        ));
        let intervening = ModRef::of(&struct_set(local_get("q"), WirInstr::I32Const(0)));
        assert!(can_move_past(&expr, &intervening, "self"));
    }

    #[test]
    fn cannot_move_heap_read_past_call() {
        // A call may mutate the heap, so a heap-reading expression
        // must not be moved past one.
        let expr = ModRef::of(&struct_get(local_get("r")));
        let intervening = ModRef::of(&call(vec![]));
        assert!(!can_move_past(&expr, &intervening, "self"));
    }

    #[test]
    fn can_move_local_only_read_past_call() {
        // Locals are private to the calling frame; callees cannot
        // touch them, so a pure local read is safe to move past a
        // call.
        let expr = ModRef::of(&local_get("x"));
        let intervening = ModRef::of(&call(vec![]));
        assert!(can_move_past(&expr, &intervening, "self"));
    }

    #[test]
    fn cannot_move_past_branch() {
        let expr = ModRef::of(&local_get("x"));
        let intervening = ModRef::of(&WirInstr::Br { depth: 0 });
        assert!(!can_move_past(&expr, &intervening, "self"));
    }

    #[test]
    fn cannot_move_past_return() {
        let expr = ModRef::of(&local_get("x"));
        let intervening = ModRef::of(&WirInstr::Return { value: None });
        assert!(!can_move_past(&expr, &intervening, "self"));
    }

    #[test]
    fn cannot_move_past_if_even_with_linear_body() {
        // `If` is `Conditional`; we conservatively reject.
        let expr = ModRef::of(&local_get("x"));
        let intervening = ModRef::of(&WirInstr::If {
            condition: Box::new(WirInstr::I32Const(1)),
            result: None,
            then_body: vec![WirInstr::Nop],
            else_body: None,
        });
        assert!(!can_move_past(&expr, &intervening, "self"));
    }

    #[test]
    fn cannot_move_when_intervening_reads_candidate() {
        // Single-use assumption would normally prevent this, but the
        // predicate must be defensive.
        let expr = ModRef::of(&WirInstr::I32Const(0));
        let intervening = ModRef::of(&local_set("y", local_get("self")));
        assert!(!can_move_past(&expr, &intervening, "self"));
    }

    #[test]
    fn cannot_move_when_both_may_trap() {
        // expr.may_trap = true (StructGet); intervening.may_trap = true (div).
        let expr = ModRef::of(&struct_get(local_get("r")));
        let intervening = ModRef::of(&local_set(
            "y",
            WirInstr::I32DivS(Box::new(local_get("a")), Box::new(local_get("b"))),
        ));
        assert!(!can_move_past(&expr, &intervening, "self"));
    }

    #[test]
    fn can_move_when_only_intervening_may_trap_and_expr_does_not() {
        let expr = ModRef::of(&WirInstr::I32Add(
            Box::new(WirInstr::I32Const(5)),
            Box::new(local_get("a")),
        ));
        let intervening = ModRef::of(&local_set(
            "y",
            WirInstr::I32DivS(Box::new(local_get("b")), Box::new(local_get("c"))),
        ));
        assert!(can_move_past(&expr, &intervening, "self"));
    }

    #[test]
    fn can_move_when_only_expr_may_trap_and_intervening_does_not() {
        let expr = ModRef::of(&struct_get(local_get("r")));
        let intervening = ModRef::of(&local_set("f", local_get("g")));
        assert!(can_move_past(&expr, &intervening, "self"));
    }
}
