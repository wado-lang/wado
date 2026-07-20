//! Struct local elimination passes for WIR.
//!
//! - **Single-field struct local elimination**: substitutes field access directly
//!   when a local holds a `StructNew { [inner] }` and is only read via `StructGet`.
//! - **Multi-field struct local elimination**: same as above but for structs with
//!   N > 1 fields where each field is accessed exactly once.
//! - **Flatten seq assignments**: canonicalizes `LocalSet(x, Seq([preamble, final]))`.
//!
//! Both elision passes substitute the initializer at the use site (or drop an
//! unread one), so beyond `is_pure_for_elision` they require the initializer
//! to be trap-free (`util::may_trap`) and the def-to-use span to be safe
//! ([`DefUseSafety`]): the def dominates every use and no initializer input
//! (local / global / linear memory) is written in between.
//!
//! Both elision passes share a single-traversal stats collector that records, per
//! local name: total `LocalGet` count, `StructGet(LocalGet(name), _)` use count,
//! def count (`LocalSet` + `LocalTee`), and per-field use counts. Validation then
//! consults this map in O(C), and substitution walks the body once more. Each
//! iteration of the fixed-point loop is O(N) in body size, replacing the previous
//! per-candidate re-walks (O(C · N)).
//!
//! The relaxed intervening-stmt variant — `elide_adjacent_single_use_struct_locals`,
//! which used `ModRef::can_move_past` — moved to NIR (`optimize/elide_box_local.rs`),
//! where it consults the alias machinery directly.
//!
//! - **Adjacent-use box elision** (`elide_adjacent_box_locals`): the same
//!   single-field substitution for a heap-reading `inner` (which
//!   `is_pure_for_elision` refuses), sound because it moves `inner` only into a
//!   single-use site whose preceding ops are all pure and unconditional. Targets
//!   the `Box<T>` locals lowering mints for `&primitive` payload bindings
//!   (`match r { Token(i) => f(*i) }`); these are born during NIR→WIR lowering
//!   (`lower::plan::boxing`), so NIR's `elide_box_local` never sees them.

use crate::hashmap::{IndexMap, IndexSet};
use crate::wir::{WirInstr, WirPackage, WirTypeDef};
use crate::wir_optimize::nullability::Nullability;
use crate::wir_optimize::util::may_trap_in;
use crate::wir_visitor::WirMutVisitor;

#[derive(Default)]
struct LocalStats {
    /// Every `LocalGet(name)` anywhere in the tree (including those wrapped in `StructGet`).
    total_localgets: u32,
    /// Every `StructGet(LocalGet(name), _)` occurrence.
    structget_uses: u32,
    /// Every `LocalSet(name, _)` or `LocalTee(name, _)` occurrence.
    defs: u32,
    /// Per-field use counts for `StructGet(LocalGet(name), field)`.
    field_uses: IndexMap<String, u32>,
}

/// Candidate def: `LocalSet(name, StructNew { type_idx, fields })`.
struct Candidate {
    type_idx: usize,
    fields: Vec<WirInstr>,
}

pub(super) fn elide_single_field_struct_locals(module: &mut WirPackage) {
    for func in &mut module.functions {
        let locals = func.declared_locals();
        let Some(body) = &mut func.body else {
            continue;
        };
        let null = Nullability::new(&locals);
        while elide_struct_locals_one_pass(body, &null) {}
    }
}

/// Whole-module pass: elide struct-typed locals for N-field structs (N > 1).
///
/// Handles patterns where a local is set from a `StructNew` with N fields and each
/// field is accessed exactly once via `StructGet`. Substitutes each field access
/// with the corresponding initializer expression and nops the original assignment.
/// This eliminates the struct type from the binary.
pub(super) fn elide_multi_field_struct_locals(module: &mut WirPackage) {
    // Collect type info needed before mutably borrowing functions.
    let type_field_names: Vec<Option<Vec<String>>> = module
        .types
        .iter()
        .map(|t| match t {
            WirTypeDef::Struct(s) => Some(s.fields.iter().map(|f| f.name.clone()).collect()),
            _ => None,
        })
        .collect();
    for func in &mut module.functions {
        let locals = func.declared_locals();
        let Some(body) = &mut func.body else {
            continue;
        };
        let null = Nullability::new(&locals);
        while elide_multi_field_struct_locals_one_pass(body, &type_field_names, &null) {}
    }
}

/// Single traversal: populate `stats` with def/use counts per local name and,
/// if `record_candidates == Some(min_fields)`, record `LocalSet(name, StructNew)`
/// with at least `min_fields` as candidate defs.
fn collect_stats(
    instr: &WirInstr,
    stats: &mut IndexMap<String, LocalStats>,
    candidates: &mut IndexMap<String, Candidate>,
    min_fields: usize,
    max_fields: usize,
) {
    match instr {
        WirInstr::LocalGet { name, .. } => {
            stats.entry(name.clone()).or_default().total_localgets += 1;
        }
        WirInstr::LocalSet { name, value } => {
            stats.entry(name.clone()).or_default().defs += 1;
            if let WirInstr::StructNew { type_id, fields } = value.as_ref()
                && fields.len() >= min_fields
                && fields.len() <= max_fields
            {
                candidates.insert(
                    name.clone(),
                    Candidate {
                        type_idx: type_id.index() as usize,
                        fields: fields.clone(),
                    },
                );
            }
            collect_stats(value, stats, candidates, min_fields, max_fields);
        }
        WirInstr::LocalTee { name, value } => {
            stats.entry(name.clone()).or_default().defs += 1;
            collect_stats(value, stats, candidates, min_fields, max_fields);
        }
        WirInstr::StructGet {
            expr, field_name, ..
        } => {
            if let WirInstr::LocalGet { name, .. } = expr.as_ref() {
                let s = stats.entry(name.clone()).or_default();
                s.total_localgets += 1;
                s.structget_uses += 1;
                *s.field_uses.entry(field_name.clone()).or_insert(0) += 1;
                // Don't recurse into expr — it's the LocalGet we just counted.
            } else {
                collect_stats(expr, stats, candidates, min_fields, max_fields);
            }
        }
        _ => {
            instr.for_each_child(&mut |child| {
                collect_stats(child, stats, candidates, min_fields, max_fields);
            });
        }
    }
}

/// Returns `true` when `instr` has no effect and no GC-heap read, so it can be
/// re-evaluated at a different program point *provided* its remaining inputs
/// (locals, globals, linear memory) are unchanged between the definition and
/// the use — [`DefUseSafety`] checks that second half per candidate.
///
/// Heap reads (`StructGet`, `ArrayGet*`) and calls are rejected because the
/// underlying GC object or array element might have been mutated between the
/// struct-local's definition and its use.  Allocations (`StructNew`, `ArrayNew*`)
/// are rejected because re-allocating creates a distinct object identity.
/// Any side-effecting instruction nested inside an expression is also rejected.
fn is_pure_for_elision(instr: &WirInstr) -> bool {
    match instr {
        // Heap reads: the field/element may have been mutated in the meantime.
        WirInstr::StructGet { .. }
        | WirInstr::ArrayGet { .. }
        | WirInstr::ArrayGetS { .. }
        | WirInstr::ArrayGetU { .. } => false,
        // Function calls: potential side effects.
        WirInstr::Call { .. } | WirInstr::CallIndirect { .. } | WirInstr::CallRef { .. } => false,
        // Heap allocations: re-allocating at the use site creates a new, distinct object.
        WirInstr::StructNew { .. }
        | WirInstr::ArrayNew { .. }
        | WirInstr::ArrayNewDefault { .. }
        | WirInstr::ArrayNewData { .. }
        | WirInstr::ArrayNewFixed { .. } => false,
        // Writes: a field initializer can assign a local/global/heap slot as a
        // side effect (e.g. `low: local.tee v(1000)`, where `v` is read
        // elsewhere). Eliding the struct would drop that write — not pure. (A
        // dropped field init must be side-effect-free; an unread field with a
        // write init must keep the struct.)
        WirInstr::LocalSet { .. }
        | WirInstr::LocalTee { .. }
        | WirInstr::GlobalSet { .. }
        | WirInstr::StructSet { .. }
        | WirInstr::ArraySet { .. } => false,
        // Everything else (constants, LocalGet, arithmetic, comparisons, …):
        // recurse to ensure no impure sub-expressions are present.
        _ => {
            let mut pure = true;
            instr.for_each_child(&mut |child| {
                if pure && !is_pure_for_elision(child) {
                    pure = false;
                }
            });
            pure
        }
    }
}

/// Walk `instr` to check whether any descendant `LocalGet(name)` has its name in
/// `candidates` and not equal to `exclude`.
fn inner_refs_any_candidate(
    instr: &WirInstr,
    candidates: &IndexSet<String>,
    exclude: &str,
) -> bool {
    if let WirInstr::LocalGet { name, .. } = instr {
        return candidates.contains(name) && name != exclude;
    }
    let mut found = false;
    instr.for_each_child(&mut |child| {
        if !found && inner_refs_any_candidate(child, candidates, exclude) {
            found = true;
        }
    });
    found
}

/// The inputs a candidate's initializer reads. Substitution re-evaluates the
/// initializer at the use site, so every input must be invariant between the
/// def and the use ([`DefUseSafety`]).
#[derive(Default)]
struct InitDeps {
    /// Locals read via `LocalGet`.
    locals: IndexSet<String>,
    /// Reads any global (`GlobalGet`).
    globals: bool,
    /// Reads linear memory or a table (loads / `table.get`).
    memory: bool,
}

impl InitDeps {
    fn collect(&mut self, instr: &WirInstr) {
        match instr {
            WirInstr::LocalGet { name, .. } => {
                self.locals.insert(name.clone());
            }
            WirInstr::GlobalGet { .. } => self.globals = true,
            WirInstr::I32Load { .. }
            | WirInstr::I32Load8U { .. }
            | WirInstr::I32Load8S { .. }
            | WirInstr::I32Load16U { .. }
            | WirInstr::I32Load16S { .. }
            | WirInstr::I64Load { .. }
            | WirInstr::V128Load { .. }
            | WirInstr::TableGet { .. } => self.memory = true,
            _ => {}
        }
        instr.for_each_child(&mut |child| self.collect(child));
    }
}

/// Per-loop event record for the back-edge invariance check: a use and a
/// clobber inside the same loop interleave across iterations (use, …,
/// clobber, back edge, use) even when the linear walk sees the use first —
/// unless the def is also re-executed inside that loop before the use.
#[derive(Default)]
struct LoopFrame {
    used: IndexSet<String>,
    clobbered: IndexSet<String>,
    defined: IndexSet<String>,
}

/// Structural def-to-use safety walk over one function body, the
/// `check_dominance_in_body` discipline from `peephole.rs` extended with
/// initializer-input invariance. A candidate survives only when:
///
/// - its def structurally dominates every use — a use reached before the def
///   would otherwise stop trapping on the null default and yield the
///   initializer's value instead;
/// - no input of its initializer is written between the def and any use
///   (including across a loop back edge): a `LocalSet`/`LocalTee`/bind of a
///   read local, a `GlobalSet` or call for a global-reading initializer, a
///   store / `memory.grow` / `memory.fill` / `table.set` or call for a
///   memory-reading one.
///
/// Evaluation order is respected where the child order diverges from it:
/// `Select` evaluates both arms before its condition.
struct DefUseSafety<'a> {
    /// Candidate name → its initializer's inputs.
    deps: &'a IndexMap<String, InitDeps>,
    /// Candidates whose def dominates the current point (mark + truncate on
    /// scope exit, as in `peephole::check_dominance_in_body`).
    dominating: Vec<String>,
    /// Candidates with an input written since their def executed.
    clobbered: IndexSet<String>,
    loop_frames: Vec<LoopFrame>,
    disqualified: IndexSet<String>,
}

impl<'a> DefUseSafety<'a> {
    fn disqualified_in(
        body: &[WirInstr],
        deps: &'a IndexMap<String, InitDeps>,
    ) -> IndexSet<String> {
        let mut walk = DefUseSafety {
            deps,
            dominating: Vec::new(),
            clobbered: IndexSet::default(),
            loop_frames: Vec::new(),
            disqualified: IndexSet::default(),
        };
        walk.check_body(body);
        walk.disqualified
    }

    fn check_body(&mut self, body: &[WirInstr]) {
        for instr in body {
            self.check_instr(instr);
        }
    }

    fn check_instr(&mut self, instr: &WirInstr) {
        if let WirInstr::StructGet { expr, .. } = instr
            && let WirInstr::LocalGet { name, .. } = expr.as_ref()
            && self.deps.contains_key(name)
        {
            self.record_use(name);
            return;
        }
        match instr {
            WirInstr::LocalSet { name, value } => {
                self.check_instr(value);
                self.clobber_local(name);
                if self.deps.contains_key(name) {
                    self.record_def(name);
                }
            }
            WirInstr::LocalTee { name, value } => {
                self.check_instr(value);
                self.clobber_local(name);
            }
            WirInstr::MultiValueLocalBind { instr, locals } => {
                self.check_instr(instr);
                for local in locals.iter().flatten() {
                    self.clobber_local(local);
                }
            }
            WirInstr::Block { body, .. } => {
                let mark = self.dominating.len();
                self.check_body(body);
                self.dominating.truncate(mark);
            }
            WirInstr::Loop { body, .. } => {
                let mark = self.dominating.len();
                self.loop_frames.push(LoopFrame::default());
                self.check_body(body);
                self.close_loop_frame();
                self.dominating.truncate(mark);
            }
            WirInstr::Seq(body) => {
                self.check_body(body);
            }
            WirInstr::If {
                condition,
                then_body,
                else_body,
                ..
            } => {
                self.check_instr(condition);
                let mark = self.dominating.len();
                self.check_body(then_body);
                self.dominating.truncate(mark);
                if let Some(eb) = else_body {
                    self.check_body(eb);
                    self.dominating.truncate(mark);
                }
            }
            WirInstr::Select {
                condition,
                if_true,
                if_false,
                ..
            } => {
                self.check_instr(if_true);
                self.check_instr(if_false);
                self.check_instr(condition);
            }
            other => {
                other.for_each_child(&mut |child| self.check_instr(child));
                self.apply_own_clobber(other);
            }
        }
    }

    /// The clobber a node's *own* operation performs after its operands have
    /// evaluated. `LocalSet`/`LocalTee`/`MultiValueLocalBind` are handled in
    /// [`Self::check_instr`]; this covers the rest.
    fn apply_own_clobber(&mut self, instr: &WirInstr) {
        match instr {
            WirInstr::GlobalSet { .. } => self.clobber(|d| d.globals),
            WirInstr::Call { .. } | WirInstr::CallIndirect { .. } | WirInstr::CallRef { .. } => {
                self.clobber(|d| d.globals || d.memory);
            }
            WirInstr::I32Store { .. }
            | WirInstr::I32Store8 { .. }
            | WirInstr::I32Store16 { .. }
            | WirInstr::I64Store { .. }
            | WirInstr::V128Store { .. }
            | WirInstr::MemoryFill { .. }
            | WirInstr::MemoryGrow(_)
            | WirInstr::TableSet { .. } => self.clobber(|d| d.memory),
            _ => {}
        }
    }

    fn clobber_local(&mut self, written: &str) {
        self.clobber(|d| d.locals.contains(written));
    }

    fn clobber(&mut self, affects: impl Fn(&InitDeps) -> bool) {
        let all_deps = self.deps;
        for (name, deps) in all_deps {
            if affects(deps) {
                self.clobbered.insert(name.clone());
                if let Some(frame) = self.loop_frames.last_mut() {
                    frame.clobbered.insert(name.clone());
                }
            }
        }
    }

    fn record_def(&mut self, name: &str) {
        self.dominating.push(name.to_string());
        self.clobbered.swap_remove(name);
        if let Some(frame) = self.loop_frames.last_mut() {
            frame.defined.insert(name.to_string());
        }
    }

    fn record_use(&mut self, name: &str) {
        if !self.dominating.iter().any(|n| n == name) || self.clobbered.contains(name) {
            self.disqualified.insert(name.to_string());
        }
        if let Some(frame) = self.loop_frames.last_mut() {
            frame.used.insert(name.to_string());
        }
    }

    /// A candidate both used and clobbered inside a loop whose def is outside
    /// it reads a different value on the iteration after the clobber.
    /// A def inside the loop re-captures per iteration, and an
    /// intra-iteration clobber between it and the use is already caught by
    /// the linear walk, so those candidates are exempt.
    fn close_loop_frame(&mut self) {
        let frame = self.loop_frames.pop().expect("balanced loop frames");
        for name in &frame.used {
            if frame.clobbered.contains(name) && !frame.defined.contains(name) {
                self.disqualified.insert(name.clone());
            }
        }
        if let Some(parent) = self.loop_frames.last_mut() {
            parent.used.extend(frame.used);
            parent.clobbered.extend(frame.clobbered);
            parent.defined.extend(frame.defined);
        }
    }
}

/// Drop candidates whose def-to-use span is unsafe ([`DefUseSafety`]).
/// `deps_of` yields the initializer expression(s) of one candidate.
fn retain_safe_candidates<V>(
    body: &[WirInstr],
    valid: &mut IndexMap<String, V>,
    deps_of: impl Fn(&V) -> InitDeps,
) {
    if valid.is_empty() {
        return;
    }
    let deps: IndexMap<String, InitDeps> = valid
        .iter()
        .map(|(name, v)| (name.clone(), deps_of(v)))
        .collect();
    let disqualified = DefUseSafety::disqualified_in(body, &deps);
    valid.retain(|name, _| !disqualified.contains(name));
}

/// One pass: collect stats, validate candidates, rewrite. Returns `true` if anything changed.
fn elide_struct_locals_one_pass(body: &mut [WirInstr], null: &Nullability) -> bool {
    let mut stats: IndexMap<String, LocalStats> = IndexMap::default();
    let mut candidates: IndexMap<String, Candidate> = IndexMap::default();
    for instr in body.iter() {
        collect_stats(instr, &mut stats, &mut candidates, 1, 1);
    }
    if candidates.is_empty() {
        return false;
    }
    let candidate_names: IndexSet<String> = candidates.keys().cloned().collect();

    // Filter to valid leaf candidates:
    //   - exactly one LocalSet/LocalTee of this name
    //   - exactly one StructGet(LocalGet(name), _) use
    //   - every LocalGet(name) is wrapped by StructGet
    //   - inner value doesn't reference another candidate
    let mut valid: IndexMap<String, WirInstr> = candidates
        .into_iter()
        .filter_map(|(name, cand)| {
            let s = stats.get(&name)?;
            if s.defs != 1 || s.structget_uses != 1 {
                return None;
            }
            if s.total_localgets != s.structget_uses {
                return None;
            }
            let inner = cand.fields.into_iter().next()?;
            if inner_refs_any_candidate(&inner, &candidate_names, &name) {
                return None;
            }
            // Only substitute when the inner expression is safe to re-evaluate at
            // the use site.  Expressions that read heap state (StructGet, ArrayGet)
            // or have side effects must not be moved past intervening mutations.
            if !is_pure_for_elision(&inner) {
                return None;
            }
            // The use may sit in a conditionally-executed position (dominance
            // does not imply post-dominance), so relocating a trap-capable
            // initializer there could skip a trap the def always fired.
            // `elide_adjacent_box_locals` still handles the trap-capable case
            // under its unconditional-adjacency discipline.
            if may_trap_in(&inner, null) {
                return None;
            }
            Some((name, inner))
        })
        .collect();

    // The remaining hazard is positional: the def must dominate the use and
    // the initializer's inputs must be unwritten in between.
    retain_safe_candidates(body, &mut valid, |inner| {
        let mut deps = InitDeps::default();
        deps.collect(inner);
        deps
    });

    if valid.is_empty() {
        return false;
    }

    for instr in body.iter_mut() {
        substitute_single_field(instr, &valid);
    }
    true
}

/// Single-pass mutator: replaces `StructGet(LocalGet(name), _)` with `inner.clone()`
/// for every valid candidate, and nops their defining `LocalSet`.
fn substitute_single_field(instr: &mut WirInstr, valid: &IndexMap<String, WirInstr>) {
    match instr {
        WirInstr::LocalSet { name, .. } if valid.contains_key(name) => {
            *instr = WirInstr::Nop;
            return;
        }
        WirInstr::StructGet { expr, .. } => {
            if let WirInstr::LocalGet { name, .. } = expr.as_ref()
                && let Some(inner) = valid.get(name)
            {
                *instr = inner.clone();
                return;
            }
        }
        _ => {}
    }
    instr.for_each_boxed_child_mut(&mut |child| substitute_single_field(child, valid));
}

/// One pass for multi-field struct local elision. Returns `true` if anything changed.
/// `type_field_names[i]` is `Some(vec_of_field_names)` if type i is a struct, else `None`.
fn elide_multi_field_struct_locals_one_pass(
    body: &mut [WirInstr],
    type_field_names: &[Option<Vec<String>>],
    null: &Nullability,
) -> bool {
    let mut stats: IndexMap<String, LocalStats> = IndexMap::default();
    let mut candidates: IndexMap<String, Candidate> = IndexMap::default();
    for instr in body.iter() {
        collect_stats(instr, &mut stats, &mut candidates, 2, usize::MAX);
    }
    if candidates.is_empty() {
        return false;
    }
    let candidate_names: IndexSet<String> = candidates.keys().cloned().collect();

    // Filter: candidate is valid when each accessed field is read exactly
    // once and every field initializer is pure. Unaccessed fields are
    // permitted (the partial-use case from `let [_rx, tx] = …`): since
    // every initializer is pure and trap-free, the unread initialiser just
    // gets dropped along with the eliminated `LocalSet`.
    let mut valid: IndexMap<String, IndexMap<String, WirInstr>> = candidates
        .into_iter()
        .filter_map(|(name, cand)| {
            let s = stats.get(&name)?;
            if s.defs != 1 {
                return None;
            }
            let struct_field_names = type_field_names.get(cand.type_idx)?.as_ref()?;
            if struct_field_names.len() != cand.fields.len() {
                return None;
            }
            // Every LocalGet(name) must be the source of a StructGet.
            if s.total_localgets != s.structget_uses {
                return None;
            }
            // Each ACCESSED field is read exactly once. Unaccessed fields
            // (count == 0) are fine.
            for fname in struct_field_names {
                let n = s.field_uses.get(fname).copied().unwrap_or(0);
                if n > 1 {
                    return None;
                }
            }
            // The total number of StructGet uses matches the sum of
            // per-field counts (sanity).
            let accessed: u32 = struct_field_names
                .iter()
                .map(|f| s.field_uses.get(f).copied().unwrap_or(0))
                .sum();
            if s.structget_uses != accessed {
                return None;
            }
            for inner in &cand.fields {
                if inner_refs_any_candidate(inner, &candidate_names, &name) {
                    return None;
                }
                // Initializers must be pure so unaccessed ones can be
                // dropped without changing program semantics.
                if !is_pure_for_elision(inner) {
                    return None;
                }
                // A trap-capable initializer can neither be dropped (an
                // unread field's trap would vanish) nor moved to a use that
                // may execute conditionally.
                if may_trap_in(inner, null) {
                    return None;
                }
            }
            let subst: IndexMap<String, WirInstr> = struct_field_names
                .iter()
                .cloned()
                .zip(cand.fields)
                .collect();
            Some((name, subst))
        })
        .collect();

    // Positional safety: every field use must be dominated by the def with
    // the union of all field initializers' inputs unwritten in between.
    retain_safe_candidates(body, &mut valid, |fields| {
        let mut deps = InitDeps::default();
        for inner in fields.values() {
            deps.collect(inner);
        }
        deps
    });

    if valid.is_empty() {
        return false;
    }

    for instr in body.iter_mut() {
        substitute_multi_field(instr, &valid);
    }
    true
}

/// Single-pass mutator for the multi-field pass.
fn substitute_multi_field(
    instr: &mut WirInstr,
    valid: &IndexMap<String, IndexMap<String, WirInstr>>,
) {
    match instr {
        WirInstr::LocalSet { name, .. } if valid.contains_key(name) => {
            *instr = WirInstr::Nop;
            return;
        }
        WirInstr::StructGet {
            expr, field_name, ..
        } => {
            if let WirInstr::LocalGet { name, .. } = expr.as_ref()
                && let Some(field_map) = valid.get(name)
                && let Some(value) = field_map.get(field_name)
            {
                *instr = value.clone();
                return;
            }
        }
        _ => {}
    }
    instr.for_each_boxed_child_mut(&mut |child| substitute_multi_field(child, valid));
}

/// Flatten `LocalSet { name, value: Seq([preamble..., final]) }` into
/// `[preamble..., LocalSet { name, value: final }]` at all levels of each function.
///
/// This canonicalizes the pattern produced by the WIR builder for tuple destructuring,
/// making it visible to downstream passes like multi-field struct local elision.
pub(super) fn flatten_seq_assignments(module: &mut WirPackage) {
    let mut visitor = FlattenSeqAssignments;
    for func in &mut module.functions {
        if let Some(body) = &mut func.body {
            visitor.visit_body(body);
        }
    }
}

struct FlattenSeqAssignments;

impl WirMutVisitor for FlattenSeqAssignments {
    fn visit_body(&mut self, body: &mut Vec<WirInstr>) {
        // First recurse into nested bodies.
        self.walk_body(body);
        // Then expand any LocalSet { value: Seq([..., final]) } at this level.
        let old = std::mem::take(body);
        for instr in old {
            match instr {
                WirInstr::LocalSet { name, value } if matches!(value.as_ref(), WirInstr::Seq(seq) if !seq.is_empty()) => {
                    if let WirInstr::Seq(mut seq) = *value {
                        let final_val = seq.pop().unwrap();
                        body.extend(seq);
                        body.push(WirInstr::LocalSet {
                            name,
                            value: Box::new(final_val),
                        });
                    }
                }
                other => body.push(other),
            }
        }
    }
}

/// Outcome of scanning a using statement in evaluation order for the box's use.
#[derive(Clone, Copy, PartialEq, Eq)]
enum BoxUseWalk {
    /// The use is reached with only pure, unconditional ops before it.
    Found,
    /// Pure subtree with no use.
    Pure,
    /// A call, write, or conditional is reached before the use, or the use is
    /// conditionally evaluated.
    Blocked,
}

/// Elide single-field (`Box<T>`) struct locals with a heap-reading initializer
/// when the def and its single use are adjacent in straight-line code — the case
/// [`elide_single_field_struct_locals`] refuses because `inner` is not pure.
/// Fix-point per function over the monotonic def/use `stats` oracle.
pub(super) fn elide_adjacent_box_locals(module: &mut WirPackage) {
    for func in &mut module.functions {
        let Some(body) = &mut func.body else {
            continue;
        };
        loop {
            let mut stats: IndexMap<String, LocalStats> = IndexMap::default();
            let mut candidates: IndexMap<String, Candidate> = IndexMap::default();
            for instr in body.iter() {
                collect_stats(instr, &mut stats, &mut candidates, 1, 1);
            }
            if candidates.is_empty() {
                break;
            }
            let candidate_names: IndexSet<String> = candidates.keys().cloned().collect();
            let mut elider = AdjacentBoxElider {
                stats: &stats,
                candidate_names: &candidate_names,
                changed: false,
            };
            elider.visit_body(body);
            if !elider.changed {
                break;
            }
        }
    }
}

struct AdjacentBoxElider<'a> {
    stats: &'a IndexMap<String, LocalStats>,
    candidate_names: &'a IndexSet<String>,
    changed: bool,
}

impl WirMutVisitor for AdjacentBoxElider<'_> {
    fn visit_body(&mut self, body: &mut Vec<WirInstr>) {
        self.walk_body(body);
        self.process_stmt_list(body);
    }
}

impl AdjacentBoxElider<'_> {
    fn process_stmt_list(&mut self, stmts: &mut [WirInstr]) {
        for p in 0..stmts.len() {
            let Some((name, field, inner)) = self.describe_box_def(&stmts[p]) else {
                continue;
            };
            let Some(k) = find_adjacent_box_use(stmts, p + 1, &name, &field) else {
                continue;
            };
            if substitute_box_use(&mut stmts[k], &name, &field, &inner) {
                stmts[p] = WirInstr::Nop;
                self.changed = true;
            }
        }
    }

    /// `LocalSet(name, StructNew{[inner]})` defined once, read once as
    /// `StructGet(LocalGet(name), field)`, with an `inner` safe to relocate to
    /// the use site (no own effect, no other-candidate read).
    fn describe_box_def(&self, stmt: &WirInstr) -> Option<(String, String, WirInstr)> {
        let WirInstr::LocalSet { name, value } = stmt else {
            return None;
        };
        let WirInstr::StructNew { fields, .. } = value.as_ref() else {
            return None;
        };
        if fields.len() != 1 {
            return None;
        }
        let s = self.stats.get(name)?;
        if s.defs != 1 || s.structget_uses != 1 || s.total_localgets != 1 {
            return None;
        }
        if s.field_uses.len() != 1 {
            return None;
        }
        let inner = &fields[0];
        if inner_has_effect(inner) || inner_refs_any_candidate(inner, self.candidate_names, name) {
            return None;
        }
        let field = s.field_uses.keys().next()?.clone();
        Some((name.clone(), field, inner.clone()))
    }
}

/// Whether `inner`'s subtree performs a call or write ([`is_effect_barrier`]).
/// Relocating such an effect past the intervening reads the adjacency walk
/// classifies as pure would reorder it; heap reads and pure computation cannot.
fn inner_has_effect(instr: &WirInstr) -> bool {
    if is_effect_barrier(instr) {
        return true;
    }
    let mut found = false;
    instr.for_each_child(&mut |c| {
        if !found && inner_has_effect(c) {
            found = true;
        }
    });
    found
}

/// Index of the use when it is the leftmost effect of the immediately-following
/// non-Nop statement — the only statement the pass reorders across (none: the
/// reorder is confined inside that one statement, per [`leftmost_box_use`]).
fn find_adjacent_box_use(
    stmts: &[WirInstr],
    from: usize,
    name: &str,
    field: &str,
) -> Option<usize> {
    let k = (from..stmts.len()).find(|&k| !matches!(stmts[k], WirInstr::Nop))?;
    match leftmost_box_use(&stmts[k], name, field) {
        BoxUseWalk::Found => Some(k),
        BoxUseWalk::Pure | BoxUseWalk::Blocked => None,
    }
}

/// Is `instr` the target `StructGet(LocalGet(name), field)`?
fn is_box_use(instr: &WirInstr, name: &str, field: &str) -> bool {
    matches!(
        instr,
        WirInstr::StructGet { field_name, expr, .. }
            if field_name == field
                && matches!(expr.as_ref(), WirInstr::LocalGet { name: n, .. } if n == name)
    )
}

/// A node whose own operation calls or writes. Its operands evaluate before that
/// effect, so a use in an operand is still valid; a use past the effect is not.
fn is_effect_barrier(instr: &WirInstr) -> bool {
    matches!(
        instr,
        WirInstr::Call { .. }
            | WirInstr::CallIndirect { .. }
            | WirInstr::CallRef { .. }
            | WirInstr::LocalSet { .. }
            | WirInstr::LocalTee { .. }
            | WirInstr::GlobalSet { .. }
            | WirInstr::StructSet { .. }
            | WirInstr::ArraySet { .. }
            | WirInstr::ArrayCopy { .. }
            | WirInstr::ArrayFill { .. }
            | WirInstr::MemoryFill { .. }
            | WirInstr::MemoryGrow(_)
            | WirInstr::TableSet { .. }
            | WirInstr::MultiValueLocalBind { .. }
            | WirInstr::I32Store { .. }
            | WirInstr::I32Store8 { .. }
            | WirInstr::I32Store16 { .. }
            | WirInstr::I64Store { .. }
            | WirInstr::V128Store { .. }
    )
}

/// A node that evaluates children conditionally or transfers control, so a use
/// inside it may run conditionally and cannot anchor an elision.
fn is_control_barrier(instr: &WirInstr) -> bool {
    matches!(
        instr,
        WirInstr::Block { .. }
            | WirInstr::Loop { .. }
            | WirInstr::If { .. }
            | WirInstr::BranchHint { .. }
            | WirInstr::Br { .. }
            | WirInstr::BrIf { .. }
            | WirInstr::BrTable { .. }
            | WirInstr::Return { .. }
            | WirInstr::Unreachable
            | WirInstr::ColdPath
            | WirInstr::Select { .. }
    )
}

/// Classify `instr` for the box use in evaluation order. Nodes not covered by
/// [`is_effect_barrier`] / [`is_control_barrier`] are pure and evaluate their
/// operands left to right; the barrier lists must stay exhaustive over every
/// call, write, and branch for the pure-by-default arm to be sound. A trap or
/// heap read before the use is harmless — the relocated initializer is effect-free.
fn leftmost_box_use(instr: &WirInstr, name: &str, field: &str) -> BoxUseWalk {
    if is_box_use(instr, name, field) {
        return BoxUseWalk::Found;
    }
    if is_control_barrier(instr) {
        return BoxUseWalk::Blocked;
    }
    let mut acc = BoxUseWalk::Pure;
    instr.for_each_child(&mut |c| {
        if acc == BoxUseWalk::Pure {
            acc = leftmost_box_use(c, name, field);
        }
    });
    match (is_effect_barrier(instr), acc) {
        (_, BoxUseWalk::Found) => BoxUseWalk::Found,
        (true, _) => BoxUseWalk::Blocked,
        (false, other) => other,
    }
}

/// Replace the box's single `StructGet(LocalGet(name), field)` use with `inner`.
fn substitute_box_use(instr: &mut WirInstr, name: &str, field: &str, inner: &WirInstr) -> bool {
    if is_box_use(instr, name, field) {
        *instr = inner.clone();
        return true;
    }
    let mut done = false;
    instr.for_each_boxed_child_mut(&mut |child| {
        if !done {
            done = substitute_box_use(child, name, field, inner);
        }
    });
    done
}

#[cfg(test)]
mod def_use_safety_tests {
    use super::*;
    use crate::wir::{WirFuncId, WirLocals, WirName, WirType, WirTypeId};
    use std::rc::Rc;

    fn tid() -> WirTypeId {
        WirTypeId::new(0, Rc::from("Box"))
    }
    fn lget(name: &str) -> WirInstr {
        WirInstr::LocalGet {
            name: name.to_string(),
            result_ty: WirType::I32,
        }
    }
    fn lset(name: &str, value: WirInstr) -> WirInstr {
        WirInstr::LocalSet {
            name: name.to_string(),
            value: Box::new(value),
        }
    }
    fn box_def(name: &str, inner: WirInstr) -> WirInstr {
        lset(
            name,
            WirInstr::StructNew {
                type_id: tid(),
                fields: vec![inner],
            },
        )
    }
    fn box_use(name: &str) -> WirInstr {
        WirInstr::Drop(Box::new(WirInstr::StructGet {
            type_id: tid(),
            field_name: "value".to_string(),
            expr: Box::new(lget(name)),
            result_ty: WirType::I32,
        }))
    }
    fn call() -> WirInstr {
        WirInstr::Call {
            func_id: WirFuncId::new(0, Rc::from("f")),
            args: vec![],
        }
    }

    /// Baseline precision: def then use with nothing in between elides.
    #[test]
    fn clean_def_use_elides() {
        let mut body = vec![box_def("b", lget("v")), box_use("b")];
        assert!(elide_struct_locals_one_pass(
            &mut body,
            &Nullability::new(&WirLocals::default())
        ));
        assert!(matches!(body[0], WirInstr::Nop));
        let WirInstr::Drop(inner) = &body[1] else {
            panic!("expected Drop");
        };
        assert!(matches!(inner.as_ref(), WirInstr::LocalGet { name, .. } if name == "v"));
    }

    /// An intervening write to the initializer's source local blocks the
    /// substitution: `b = S{v}; v = 9; use b.value` must keep reading the
    /// value captured at the def.
    #[test]
    fn intervening_local_write_blocks() {
        let mut body = vec![
            box_def("b", lget("v")),
            lset("v", WirInstr::I32Const(9)),
            box_use("b"),
        ];
        assert!(!elide_struct_locals_one_pass(
            &mut body,
            &Nullability::new(&WirLocals::default())
        ));
        assert!(matches!(body[0], WirInstr::LocalSet { .. }));
    }

    /// A use reached before the def would trade the null-receiver trap for
    /// the initializer's value; the dominance walk must reject it.
    #[test]
    fn use_before_def_blocks() {
        let mut body = vec![box_use("b"), box_def("b", WirInstr::I32Const(1))];
        assert!(!elide_struct_locals_one_pass(
            &mut body,
            &Nullability::new(&WirLocals::default())
        ));
    }

    /// A def inside one `if` arm does not dominate a use after the `if`.
    #[test]
    fn conditional_def_blocks() {
        let mut body = vec![
            WirInstr::If {
                condition: Box::new(lget("c")),
                result: None,
                then_body: vec![box_def("b", WirInstr::I32Const(1))],
                else_body: None,
            },
            box_use("b"),
        ];
        assert!(!elide_struct_locals_one_pass(
            &mut body,
            &Nullability::new(&WirLocals::default())
        ));
    }

    /// Loop back edge: def before the loop, use and source-write inside it —
    /// iteration 2's use would read the clobbered source.
    #[test]
    fn loop_back_edge_clobber_blocks() {
        let mut body = vec![
            box_def("b", lget("v")),
            WirInstr::Loop {
                label: None,
                body: vec![box_use("b"), lset("v", WirInstr::I32Const(9))],
            },
        ];
        assert!(!elide_struct_locals_one_pass(
            &mut body,
            &Nullability::new(&WirLocals::default())
        ));
    }

    /// Precision: a def *inside* the loop re-captures per iteration, so a
    /// write after the use in the same iteration is harmless.
    #[test]
    fn loop_local_def_after_use_write_elides() {
        let mut body = vec![WirInstr::Loop {
            label: None,
            body: vec![
                box_def("b", lget("v")),
                box_use("b"),
                lset("v", WirInstr::I32Const(9)),
            ],
        }];
        assert!(elide_struct_locals_one_pass(
            &mut body,
            &Nullability::new(&WirLocals::default())
        ));
    }

    /// A call between the def and the use clobbers a global-reading
    /// initializer (the callee may write the global) …
    #[test]
    fn call_clobbers_global_reader() {
        let g = WirInstr::GlobalGet {
            name: WirName {
                fq: "g".to_string(),
            },
            result_ty: WirType::I32,
        };
        let mut body = vec![box_def("b", g.clone()), call(), box_use("b")];
        assert!(!elide_struct_locals_one_pass(
            &mut body,
            &Nullability::new(&WirLocals::default())
        ));
        // … but with nothing in between the same initializer elides.
        let mut body = vec![box_def("b", g), box_use("b")];
        assert!(elide_struct_locals_one_pass(
            &mut body,
            &Nullability::new(&WirLocals::default())
        ));
    }

    /// A call never writes locals, so it does not clobber a `LocalGet`
    /// initializer (precision guard for the `sroa_param` call-site shape).
    #[test]
    fn call_does_not_clobber_local_reader() {
        let mut body = vec![box_def("b", lget("v")), call(), box_use("b")];
        assert!(elide_struct_locals_one_pass(
            &mut body,
            &Nullability::new(&WirLocals::default())
        ));
    }

    /// `Select` evaluates its arms before its condition: a def in the
    /// condition runs after a use in an arm, so it must not count as
    /// dominating.
    #[test]
    fn select_arm_use_with_condition_def_blocks() {
        let use_expr = WirInstr::StructGet {
            type_id: tid(),
            field_name: "value".to_string(),
            expr: Box::new(lget("b")),
            result_ty: WirType::I32,
        };
        let def_then_cond = WirInstr::Seq(vec![
            box_def("b", WirInstr::I32Const(1)),
            WirInstr::I32Const(0),
        ]);
        let mut body = vec![WirInstr::Drop(Box::new(WirInstr::Select {
            condition: Box::new(def_then_cond),
            if_true: Box::new(use_expr),
            if_false: Box::new(WirInstr::I32Const(7)),
            ty: None,
        }))];
        assert!(!elide_struct_locals_one_pass(
            &mut body,
            &Nullability::new(&WirLocals::default())
        ));
    }

    /// A trap-capable initializer must not be relocated by the strict pass
    /// (the adjacency pass still covers it under its own discipline).
    #[test]
    fn trapping_initializer_blocks() {
        let div = WirInstr::I32DivS(Box::new(lget("x")), Box::new(lget("y")));
        let mut body = vec![box_def("b", div), box_use("b")];
        assert!(!elide_struct_locals_one_pass(
            &mut body,
            &Nullability::new(&WirLocals::default())
        ));
    }

    /// The multi-field pass must not drop an unread trapping initializer.
    #[test]
    fn multi_field_unread_trapping_initializer_blocks() {
        let div = WirInstr::I32DivS(Box::new(lget("x")), Box::new(lget("y")));
        let mut body = vec![
            lset(
                "p",
                WirInstr::StructNew {
                    type_id: tid(),
                    fields: vec![div, WirInstr::I32Const(7)],
                },
            ),
            WirInstr::Drop(Box::new(WirInstr::StructGet {
                type_id: tid(),
                field_name: "b".to_string(),
                expr: Box::new(lget("p")),
                result_ty: WirType::I32,
            })),
        ];
        let field_names = vec![Some(vec!["a".to_string(), "b".to_string()])];
        assert!(!elide_multi_field_struct_locals_one_pass(
            &mut body,
            &field_names,
            &Nullability::new(&WirLocals::default())
        ));
        // Trap-free initializers with the same shape still elide.
        let mut body = vec![
            lset(
                "p",
                WirInstr::StructNew {
                    type_id: tid(),
                    fields: vec![lget("x"), WirInstr::I32Const(7)],
                },
            ),
            WirInstr::Drop(Box::new(WirInstr::StructGet {
                type_id: tid(),
                field_name: "b".to_string(),
                expr: Box::new(lget("p")),
                result_ty: WirType::I32,
            })),
        ];
        assert!(elide_multi_field_struct_locals_one_pass(
            &mut body,
            &field_names,
            &Nullability::new(&WirLocals::default())
        ));
    }
}

#[cfg(test)]
mod adjacent_box_tests {
    use super::*;
    use crate::wir::{WirFuncId, WirType, WirTypeId};
    use std::rc::Rc;

    const NAME: &str = "b";
    const FIELD: &str = "value";

    fn tid() -> WirTypeId {
        WirTypeId::new(0, Rc::from("Box"))
    }
    fn lget(name: &str) -> WirInstr {
        WirInstr::LocalGet {
            name: name.to_string(),
            result_ty: WirType::I32,
        }
    }
    /// The target `StructGet(LocalGet("b"), "value")`.
    fn use_box() -> WirInstr {
        WirInstr::StructGet {
            type_id: tid(),
            field_name: FIELD.to_string(),
            expr: Box::new(lget(NAME)),
            result_ty: WirType::I32,
        }
    }
    fn call(args: Vec<WirInstr>) -> WirInstr {
        WirInstr::Call {
            func_id: WirFuncId::new(0, Rc::from("f")),
            args,
        }
    }
    fn walk(instr: &WirInstr) -> BoxUseWalk {
        leftmost_box_use(instr, NAME, FIELD)
    }

    /// `f(other, *b)` — the use is a later call arg after a pure `LocalGet`.
    /// Found: nothing side-effecting runs before the use.
    #[test]
    fn found_use_after_pure_arg() {
        let instr = call(vec![lget("other"), use_box()]);
        assert!(matches!(walk(&instr), BoxUseWalk::Found));
    }

    /// `sum + *b` — the use is the right operand of a pure add. Found.
    #[test]
    fn found_use_in_arithmetic() {
        let instr = WirInstr::I32Add(Box::new(lget("sum")), Box::new(use_box()));
        assert!(matches!(walk(&instr), BoxUseWalk::Found));
    }

    /// `f(g(), *b)` — a call is evaluated before the use, and it could mutate
    /// what the boxed initializer reads. Blocked.
    #[test]
    fn blocked_by_call_before_use() {
        let instr = call(vec![call(vec![]), use_box()]);
        assert!(matches!(walk(&instr), BoxUseWalk::Blocked));
    }

    /// `*b` nested inside an `If` — the use is conditionally evaluated, so
    /// moving the (possibly-trapping) box init there could drop a trap. Blocked.
    #[test]
    fn blocked_by_conditional() {
        let instr = WirInstr::If {
            condition: Box::new(WirInstr::I32Const(1)),
            result: Some(WirType::I32),
            then_body: vec![use_box()],
            else_body: Some(vec![WirInstr::I32Const(0)]),
        };
        assert!(matches!(walk(&instr), BoxUseWalk::Blocked));
    }

    /// A heap write before the use blocks: the write could change what the
    /// boxed read observes if moved after it.
    #[test]
    fn blocked_by_heap_write_before_use() {
        let write = WirInstr::StructSet {
            type_id: tid(),
            field_name: "f".to_string(),
            expr: Box::new(lget("obj")),
            value: Box::new(WirInstr::I32Const(1)),
        };
        // `(struct.set …, *b)` modelled as a two-arg call so both are siblings.
        let instr = call(vec![write, use_box()]);
        assert!(matches!(walk(&instr), BoxUseWalk::Blocked));
    }

    /// A statement with no box use and no side effect is Pure — the scan may
    /// safely skip it (it never does in practice, but the classification must
    /// not report a spurious barrier).
    #[test]
    fn pure_when_no_use() {
        let instr = WirInstr::I32Add(Box::new(lget("x")), Box::new(WirInstr::I32Const(1)));
        assert!(matches!(walk(&instr), BoxUseWalk::Pure));
    }

    /// A heap read (non-target `StructGet`) before the use is fine — reads
    /// never change what another read observes, and a trap aborts before the
    /// pure moved read matters. Found.
    #[test]
    fn found_use_after_heap_read() {
        let other_read = WirInstr::StructGet {
            type_id: tid(),
            field_name: "other".to_string(),
            expr: Box::new(lget("obj")),
            result_ty: WirType::I32,
        };
        let instr = call(vec![other_read, use_box()]);
        assert!(matches!(walk(&instr), BoxUseWalk::Found));
    }

    /// The moved initializer must be free of calls and writes: a heap read is
    /// relocatable, but a `Call` or a `LocalTee`/write is not (its effect would
    /// be reordered past the intervening pure reads the walk allows).
    #[test]
    fn inner_effect_gate() {
        // pure heap read (the intended payload-read case) → relocatable
        let read = WirInstr::StructGet {
            type_id: tid(),
            field_name: "payload_0".to_string(),
            expr: Box::new(lget("scrut")),
            result_ty: WirType::I32,
        };
        assert!(!inner_has_effect(&read));
        // a call → not relocatable
        assert!(inner_has_effect(&call(vec![])));
        // a call nested inside otherwise-pure arithmetic → not relocatable
        let nested = WirInstr::I32Add(Box::new(lget("x")), Box::new(call(vec![])));
        assert!(inner_has_effect(&nested));
        // a local.tee (write) → not relocatable
        let tee = WirInstr::LocalTee {
            name: "v".to_string(),
            value: Box::new(WirInstr::I32Const(1)),
        };
        assert!(inner_has_effect(&tee));
    }

    /// End-to-end substitution: `substitute_box_use` replaces the single use
    /// with the initializer and reports success.
    #[test]
    fn substitute_replaces_single_use() {
        let inner = WirInstr::I32Const(42);
        let mut instr = call(vec![lget("other"), use_box()]);
        assert!(substitute_box_use(&mut instr, NAME, FIELD, &inner));
        let WirInstr::Call { args, .. } = &instr else {
            panic!("expected call");
        };
        assert!(matches!(args[1], WirInstr::I32Const(42)));
    }
}
