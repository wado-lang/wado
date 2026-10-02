//! Shared utility functions for WIR optimization passes.

use crate::hashmap::{IndexMap, IndexSet};
use crate::wir::{WirExportDesc, WirInstr, WirLocals, WirPackage, WirType, WirTypeId};

use super::nullability::Nullability;

/// `func_id`s that must not be SROA'd: exports, element tables, and `RefFunc`
/// references.
pub(super) fn collect_pinned_func_ids(module: &WirPackage) -> IndexSet<u32> {
    let mut pinned = IndexSet::default();

    // Exported functions
    for export in &module.exports {
        if let WirExportDesc::Func { func_id } = &export.desc {
            pinned.insert(func_id.index());
        }
    }

    // Element table functions
    for elem in &module.elements {
        for fid in &elem.func_ids {
            pinned.insert(fid.index());
        }
    }

    // RefFunc references in all function bodies
    for func in &module.functions {
        if let Some(body) = &func.body {
            collect_ref_funcs(body, &mut pinned);
        }
    }

    // Also check global initializers for RefFunc
    for global in &module.globals {
        collect_ref_funcs_instr(&global.init, &mut pinned);
    }

    pinned
}

fn collect_ref_funcs(instrs: &[WirInstr], pinned: &mut IndexSet<u32>) {
    for instr in instrs {
        collect_ref_funcs_instr(instr, pinned);
    }
}

fn collect_ref_funcs_instr(instr: &WirInstr, pinned: &mut IndexSet<u32>) {
    if let WirInstr::RefFunc { func_id } = instr {
        pinned.insert(func_id.index());
    }
    instr.for_each_child(&mut |child| collect_ref_funcs_instr(child, pinned));
}

/// The state a subtree reads or writes: locals and struct fields by name,
/// the rest of the heap as one region, and a call as any of it.
#[derive(Default)]
pub(super) struct Footprint {
    locals: IndexSet<String>,
    /// Struct fields of any type: aliasing is by name.
    fields: IndexSet<String>,
    /// Arrays, globals, linear memory and tables.
    heap: bool,
    call: bool,
}

impl Footprint {
    pub(super) fn reads(instr: &WirInstr) -> Self {
        let mut footprint = Self::default();
        footprint.add_reads(instr);
        footprint
    }

    pub(super) fn writes(instr: &WirInstr) -> Self {
        Self::writes_all(std::slice::from_ref(instr))
    }

    pub(super) fn writes_all(body: &[WirInstr]) -> Self {
        let mut footprint = Self::default();
        for instr in body {
            footprint.add_writes(instr);
        }
        footprint
    }

    /// What `instr` itself writes, its operands left out.
    pub(super) fn writes_of_node(instr: &WirInstr) -> Self {
        let mut footprint = Self::default();
        footprint.add_node_writes(instr);
        footprint
    }

    pub(super) fn overlaps(&self, other: &Self) -> bool {
        self.locals.iter().any(|l| other.locals.contains(l))
            || self.fields.iter().any(|f| other.fields.contains(f))
            || (self.heap && other.heap)
            || (self.call && other.touches_heap())
            || (other.call && self.touches_heap())
    }

    pub(super) fn has_local(&self, name: &str) -> bool {
        self.locals.contains(name)
    }

    pub(super) fn may_have_field(&self, name: &str) -> bool {
        self.call || self.fields.contains(name)
    }

    fn touches_heap(&self) -> bool {
        self.heap || self.call || !self.fields.is_empty()
    }

    fn add_reads(&mut self, instr: &WirInstr) {
        if let Some(name) = instr.local_read() {
            self.locals.insert(name.to_string());
        }
        match instr {
            WirInstr::StructGet { field_name, .. } => {
                self.fields.insert(field_name.clone());
            }
            WirInstr::GlobalGet { .. }
            | WirInstr::ArrayGet { .. }
            | WirInstr::ArrayGetS { .. }
            | WirInstr::ArrayGetU { .. }
            | WirInstr::ArrayLen(_)
            | WirInstr::ArrayCopy { .. }
            | WirInstr::I32Load { .. }
            | WirInstr::I32Load8U { .. }
            | WirInstr::I32Load8S { .. }
            | WirInstr::I32Load16U { .. }
            | WirInstr::I32Load16S { .. }
            | WirInstr::I64Load { .. }
            | WirInstr::V128Load { .. }
            | WirInstr::TableGet { .. }
            | WirInstr::MemorySize => self.heap = true,
            WirInstr::Call { .. } | WirInstr::CallIndirect { .. } | WirInstr::CallRef { .. } => {
                self.call = true;
            }
            _ => {}
        }
        instr.for_each_child(&mut |child| self.add_reads(child));
    }

    fn add_writes(&mut self, instr: &WirInstr) {
        self.add_node_writes(instr);
        instr.for_each_child(&mut |child| self.add_writes(child));
    }

    fn add_node_writes(&mut self, instr: &WirInstr) {
        self.locals.extend(instr.local_writes().map(str::to_string));
        match instr {
            WirInstr::StructSet { field_name, .. } => {
                self.fields.insert(field_name.clone());
            }
            WirInstr::GlobalSet { .. }
            | WirInstr::ArraySet { .. }
            | WirInstr::ArrayCopy { .. }
            | WirInstr::ArrayFill { .. }
            | WirInstr::TableSet { .. }
            | WirInstr::I32Store { .. }
            | WirInstr::I32Store8 { .. }
            | WirInstr::I32Store16 { .. }
            | WirInstr::I64Store { .. }
            | WirInstr::V128Store { .. }
            | WirInstr::MemoryGrow(_)
            | WirInstr::MemoryFill { .. } => self.heap = true,
            WirInstr::Call { .. } | WirInstr::CallIndirect { .. } | WirInstr::CallRef { .. } => {
                self.call = true;
            }
            _ => {}
        }
    }
}

/// True if no node in `instr`'s sub-tree is observable. Pure loads
/// (`StructGet`, `ArrayGet*`, memory loads, `LocalGet`, `GlobalGet`) and
/// arithmetic / ref ops are treated as side-effect-free.
pub(super) fn is_side_effect_free(instr: &WirInstr) -> bool {
    if is_root_observable(instr) {
        return false;
    }
    let mut ok = true;
    instr.for_each_child(&mut |child| {
        if ok && !is_side_effect_free(child) {
            ok = false;
        }
    });
    ok
}

/// True if the *root* of `instr` would change observable program behavior on
/// its own. Covers explicit state mutation (heap / global / local / table),
/// calls (potentially I/O), the explicit [`WirInstr::Unreachable`] trap,
/// control-flow exits that bypass subsequent siblings, and the
/// [`WirInstr::BlackBox`] barrier, which mutates nothing but must survive. Does
/// **not** classify implicit-trap ops (integer divide / remainder, OOB heap
/// reads / loads, null `ref.as_non_null` / `ref.cast`, etc.) as observable.
///
/// Does not look at children; combine with recursion (see
/// [`is_side_effect_free`]) for tree purity.
pub(super) fn is_root_observable(instr: &WirInstr) -> bool {
    matches!(
        instr,
        // Calls.
        WirInstr::Call { .. }
        | WirInstr::CallIndirect { .. }
        | WirInstr::CallRef { .. }
        // A dropped `black_box(work())` is what keeps `work()` in the program.
        | WirInstr::BlackBox(_)
        // GC / local / global state mutation.
        | WirInstr::LocalSet { .. }
        | WirInstr::LocalTee { .. }
        | WirInstr::GlobalSet { .. }
        | WirInstr::StructSet { .. }
        | WirInstr::ArraySet { .. }
        | WirInstr::ArrayCopy { .. }
        | WirInstr::ArrayFill { .. }
        | WirInstr::TableSet { .. }
        | WirInstr::MultiValueLocalBind { .. }
        // Linear-memory writes.
        | WirInstr::I32Store { .. }
        | WirInstr::I32Store8 { .. }
        | WirInstr::I32Store16 { .. }
        | WirInstr::I64Store { .. }
        | WirInstr::V128Store { .. }
        | WirInstr::MemoryGrow(_)
        | WirInstr::MemoryFill { .. }
        // Trap.
        | WirInstr::Unreachable
        // Control-flow exits — execution of a sub-expression that contains
        // these is observable because the branch transfers control past
        // siblings that would otherwise execute.
        | WirInstr::Br { .. }
        | WirInstr::BrIf { .. }
        | WirInstr::BrTable { .. }
        | WirInstr::Return { .. }
    )
}

/// What an instruction traps on by itself, apart from what its operands do.
pub(super) struct OwnTraps {
    /// The operands, by position in evaluation order, it traps on when null.
    on_null: [Option<usize>; 2],
    /// Whether it can also trap for another reason: an index out of bounds, a
    /// failed cast, a zero divisor, an explicit trap.
    otherwise: bool,
}

impl OwnTraps {
    const fn new(on_null: &[usize], otherwise: bool) -> Self {
        let mut slots = [None, None];
        let mut i = 0;
        while i < on_null.len() {
            slots[i] = Some(on_null[i]);
            i += 1;
        }
        Self {
            on_null: slots,
            otherwise,
        }
    }

    /// The positions of the operands it traps on when null, in evaluation order.
    pub(super) fn on_null(&self) -> impl Iterator<Item = usize> {
        self.on_null.into_iter().flatten()
    }
}

/// How `instr` itself traps.
pub(super) fn own_traps(instr: &WirInstr) -> OwnTraps {
    match instr {
        WirInstr::StructGet { .. }
        | WirInstr::StructSet { .. }
        | WirInstr::ArrayLen(_)
        | WirInstr::RefAsNonNull(_)
        | WirInstr::I31GetS(_)
        | WirInstr::I31GetU(_) => OwnTraps::new(&[0], false),
        WirInstr::ArrayGet { .. }
        | WirInstr::ArrayGetS { .. }
        | WirInstr::ArrayGetU { .. }
        | WirInstr::ArraySet { .. }
        | WirInstr::ArrayFill { .. }
        | WirInstr::RefCast {
            nullable: false, ..
        } => OwnTraps::new(&[0], true),
        WirInstr::ArrayCopy { .. } => OwnTraps::new(&[0, 2], true),
        // The function reference is evaluated after every argument.
        WirInstr::CallRef { args, .. } => OwnTraps::new(&[args.len()], false),
        // `array.new_data` is absent although Wasm traps when offset + len
        // overruns the segment: its only two producers —
        // `translate_packed_array` and `promote_constant_arrays_to_data` —
        // emit offset 0 with the registered segment's exact length, so the
        // shape cannot overrun.
        WirInstr::RefCast { nullable: true, .. }
        | WirInstr::I32DivS(_, _)
        | WirInstr::I32DivU(_, _)
        | WirInstr::I32RemS(_, _)
        | WirInstr::I32RemU(_, _)
        | WirInstr::I64DivS(_, _)
        | WirInstr::I64DivU(_, _)
        | WirInstr::I64RemS(_, _)
        | WirInstr::I64RemU(_, _)
        | WirInstr::I32Load { .. }
        | WirInstr::I32Load8U { .. }
        | WirInstr::I32Load8S { .. }
        | WirInstr::I32Load16U { .. }
        | WirInstr::I32Load16S { .. }
        | WirInstr::I64Load { .. }
        | WirInstr::V128Load { .. }
        | WirInstr::I32Store { .. }
        | WirInstr::I32Store8 { .. }
        | WirInstr::I32Store16 { .. }
        | WirInstr::I64Store { .. }
        | WirInstr::V128Store { .. }
        | WirInstr::MemoryFill { .. }
        | WirInstr::TableGet { .. }
        | WirInstr::TableSet { .. }
        | WirInstr::Unreachable => OwnTraps::new(&[], true),
        _ => OwnTraps::new(&[], false),
    }
}

/// True if `instr` or any descendant can trap at runtime, which keeps a
/// `Drop(value)` whose value is otherwise side-effect-free: Wado requires the
/// trap of `let _ = arr[-1]` to be observable. Distinct from
/// [`is_root_observable`], kept lax so CSE can still touch a trapping operation
/// whose result *is* used. Nullability comes from the [`Nullability`] oracle.
pub(super) fn may_trap_in(instr: &WirInstr, null: &Nullability) -> bool {
    // `ref.cast T(struct.new T { … })` is identity (`struct.new` always
    // produces exactly `T`), so the cast can't trap. Only handles the direct
    // operand; tracing through `LocalGet` would need def-use analysis.
    if let WirInstr::RefCast { type_id, expr, .. } = instr
        && let WirInstr::StructNew {
            type_id: src_type, ..
        } = expr.as_ref()
        && src_type == type_id
    {
        return may_trap_in(expr, null);
    }
    let own = own_traps(instr);
    let mut trap = own.otherwise;
    let mut position = 0;
    instr.for_each_child(&mut |child| {
        trap = trap
            || may_trap_in(child, null)
            || (own.on_null().any(|p| p == position) && !null.is_nonnull(child));
        position += 1;
    });
    trap
}

/// Visit each operand `instr` traps on when null, with its position in
/// evaluation order.
pub(super) fn for_each_null_trapping_operand(
    instr: &mut WirInstr,
    mut f: impl FnMut(usize, &mut WirInstr),
) {
    let own = own_traps(instr);
    if own.on_null().next().is_none() {
        return;
    }
    let mut position = 0;
    instr.for_each_boxed_child_mut(&mut |operand| {
        if own.on_null().any(|p| p == position) {
            f(position, operand);
        }
        position += 1;
    });
}

/// Index of the first non-`Nop` statement at or after `from`.
pub(super) fn next_non_nop(stmts: &[WirInstr], from: usize) -> Option<usize> {
    (from..stmts.len()).find(|&k| !matches!(stmts[k], WirInstr::Nop))
}

/// Both operands are the same constant or the same local read, so evaluating
/// one twice costs nothing and yields the same value.
pub(super) fn is_same_free_read(a: &WirInstr, b: &WirInstr) -> bool {
    match (a, b) {
        (WirInstr::LocalGet { name: x, .. }, WirInstr::LocalGet { name: y, .. }) => x == y,
        (WirInstr::I32Const(x), WirInstr::I32Const(y)) => x == y,
        (WirInstr::I64Const(x), WirInstr::I64Const(y)) => x == y,
        _ => false,
    }
}

/// Visit each operand of `instr` that accepts `(ref null $type)` where its
/// type allows `(ref $type)`: one an instruction traps on when null by itself,
/// `ref.cast` and `ref.test`, a value dropped, tested for null, compared or
/// externalized, and one stored into a local `locals` declares nullable.
pub(super) fn for_each_nullable_ref_operand(
    instr: &mut WirInstr,
    locals: &WirLocals,
    mut f: impl FnMut(&mut WirInstr),
) {
    match instr {
        WirInstr::Drop(value) | WirInstr::RefIsNull(value) | WirInstr::ExternExternalize(value) => {
            f(value);
        }
        WirInstr::RefEq(a, b) => {
            f(a);
            f(b);
        }
        WirInstr::LocalSet { name, value } | WirInstr::LocalTee { name, value }
            if locals.is_nullable_ref(name) =>
        {
            f(value);
        }
        WirInstr::RefCast { expr, .. } | WirInstr::RefTest { expr, .. } => f(expr),
        // Relaxing what it narrows would only move the narrowing inside it.
        WirInstr::RefAsNonNull(_) => {}
        _ => for_each_null_trapping_operand(instr, |_, operand| f(operand)),
    }
}

/// Count every `LocalGet` in an expression tree, per local name.
pub(super) fn count_local_gets(instr: &WirInstr, counts: &mut IndexMap<String, u32>) {
    if let WirInstr::LocalGet { name, .. } = instr {
        *counts.entry(name.clone()).or_insert(0) += 1;
    }
    instr.for_each_child(&mut |child| {
        count_local_gets(child, counts);
    });
}

/// Visit every `WirTypeId` slot nested in a `WirType`.
pub(super) fn for_each_type_id_in_wir_type(ty: &mut WirType, f: &mut impl FnMut(&mut WirTypeId)) {
    match ty {
        WirType::Ref { type_id, .. } | WirType::Enum { type_id } | WirType::Flags { type_id } => {
            f(type_id);
        }
        _ => {}
    }
}

/// Visit every `WirTypeId` slot in `instr`'s subtree: the type-bearing
/// instruction fields plus ids nested in each node's value types
/// ([`WirInstr::for_each_value_type_mut`]). The single authority both DCE walks
/// (type-reachability collection and post-compaction remapping) derive from,
/// so a type-bearing variant cannot be visited by one and missed by the other.
pub(super) fn for_each_type_id_slot(instr: &mut WirInstr, f: &mut impl FnMut(&mut WirTypeId)) {
    instr.for_each_value_type_mut(&mut |ty| for_each_type_id_in_wir_type(ty, f));
    match instr {
        WirInstr::StructNew { type_id, .. }
        | WirInstr::StructGet { type_id, .. }
        | WirInstr::StructSet { type_id, .. }
        | WirInstr::ArrayNew { type_id, .. }
        | WirInstr::ArrayNewDefault { type_id, .. }
        | WirInstr::ArrayNewData { type_id, .. }
        | WirInstr::ArrayNewFixed { type_id, .. }
        | WirInstr::ArrayGet { type_id, .. }
        | WirInstr::ArrayGetS { type_id, .. }
        | WirInstr::ArrayGetU { type_id, .. }
        | WirInstr::ArraySet { type_id, .. }
        | WirInstr::ArrayFill { type_id, .. }
        | WirInstr::RefCast { type_id, .. }
        | WirInstr::RefTest { type_id, .. }
        | WirInstr::CallIndirect { type_id, .. }
        | WirInstr::CallRef { type_id, .. } => {
            f(type_id);
        }
        WirInstr::ArrayCopy {
            dest_type_id,
            src_type_id,
            ..
        } => {
            f(dest_type_id);
            f(src_type_id);
        }
        _ => {}
    }
    instr.for_each_boxed_child_mut(&mut |child| {
        for_each_type_id_slot(child, f);
    });
}
