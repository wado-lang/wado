//! Constant global-initializer promotion for WIR.
//!
//! A user-immutable global (`global X = …`, not `global mut`) whose
//! initializer is not a syntactic Wasm constant is extracted by
//! `lower::plan::globals::extract` into an `__initialize_module` runtime
//! assignment, leaving the Wasm global mutable with a `null`/zero
//! placeholder. After NIR optimization (`array_literal`, `string_push`,
//! `const_folding`, …) collapses builder sequences, that runtime
//! assignment frequently reduces to a constant `struct.new` /
//! `array.new_fixed` / scalar.
//!
//! This pass is the single eager/lazy classifier, realizing the "lazy iff
//! optimize could not simplify it" rule at the point where the value is
//! already correctly lowered to WIR (variant representation, non-null field
//! wrapping all baked in). For each such global it moves the constant value
//! into the global's eager `init`, marks the global immutable, and drops the
//! now-redundant `GlobalSet`(s). The dead init temps, emptied
//! `__initialize_module` body, and `__modules_initialized` guard are
//! reclaimed by `init_guard` / `dce` / `cleanup` in the same phase.
//!
//! The init assignment is reached wherever it ends up: a standalone
//! `__initialize_module` body, or — when that module-init is inlined into
//! the entry points — nested inside an `__inline___initialize_modules`
//! guard block and *duplicated* once per entry export. The scan therefore
//! recurses into nested instructions and treats a global as promotable when
//! every assignment to it is constant (they are identical copies of one
//! init, since a user-immutable global is assigned exactly once in source).
//!
//! It subsumes the former NIR `const_global_promotion` (scalar-only):
//! const-ness is decided once here, via [`WirInstr::is_const_expressible`].
//! Strings stay lazy — a `String`'s `array.new_data<u8>` repr is not a valid
//! Wasm constant instruction, so it is excluded by that predicate.

use super::dedupe_const_globals::const_key;
use crate::hashmap::{IndexMap, IndexSet};
use crate::wir::{WirInstr, WirPackage};

/// Per-candidate promotion state accumulated while scanning all assignments.
enum Consensus {
    /// Every assignment seen so far is constant; carries the value to
    /// install (all assignments are identical copies of one source init).
    Const(WirInstr),
    /// At least one assignment is non-constant: the global needs runtime
    /// init and must not be promoted.
    Disqualified,
}

pub(super) fn promote_const_global_inits(module: &mut WirPackage) {
    // Candidate globals: user-immutable (`!wado_mutable`) but currently
    // Wasm-mutable, i.e. their initializer was extracted to a runtime
    // assignment. `global mut` and already-eager globals are excluded.
    let candidate_idx: IndexMap<String, usize> = module
        .globals
        .iter()
        .enumerate()
        .filter(|(_, g)| g.mutable && !g.wado_mutable)
        .map(|(i, g)| (g.name.fq.clone(), i))
        .collect();
    if candidate_idx.is_empty() {
        return;
    }

    // Collect every assignment to a candidate (recursively, since the init
    // may be inlined into a nested guard block), folding into a per-global
    // consensus.
    let mut state: IndexMap<usize, Consensus> = IndexMap::default();
    for func in &module.functions {
        if let Some(body) = &func.body {
            for instr in body {
                collect_assignments(instr, &candidate_idx, &mut state);
            }
        }
    }

    // Promote globals whose every assignment is constant; collect their
    // names so the now-redundant `GlobalSet`s can be dropped.
    let mut promoted: IndexSet<String> = IndexSet::default();
    for (g_idx, consensus) in state {
        if let Consensus::Const(value) = consensus {
            let global = &mut module.globals[g_idx];
            global.init = value;
            global.mutable = false;
            // `lazy_init` and slot nullability are left as `register_globals`
            // set them: the slot stays nullable (a non-null const init is a
            // valid subtype) and codegen keeps narrowing reads with
            // `ref.as_non_null`, which is correct since the eager value is
            // non-null.
            promoted.insert(global.name.fq.clone());
        }
    }
    if promoted.is_empty() {
        return;
    }

    for func in &mut module.functions {
        if let Some(body) = &mut func.body {
            // Reads of every local across the whole function, so dropping a
            // promoted set's `Seq` value never discards a binding read elsewhere.
            let mut read_counts: IndexMap<String, u32> = IndexMap::default();
            for instr in body.iter() {
                count_local_reads(instr, &mut read_counts);
            }
            for instr in body.iter_mut() {
                nop_promoted_assignments(instr, &promoted, &read_counts);
            }
        }
    }
}

/// Count `LocalGet` reads per local name in a subtree.
fn count_local_reads(instr: &WirInstr, counts: &mut IndexMap<String, u32>) {
    if let WirInstr::LocalGet { name, .. } = instr {
        *counts.entry(name.clone()).or_default() += 1;
    }
    instr.for_each_child(&mut |child| count_local_reads(child, counts));
}

/// Fold a `GlobalSet` to a candidate into its consensus; recurse into
/// children otherwise. A non-const assignment poisons the candidate.
fn collect_assignments(
    instr: &WirInstr,
    candidate_idx: &IndexMap<String, usize>,
    state: &mut IndexMap<usize, Consensus>,
) {
    if let WirInstr::GlobalSet { name, value } = instr
        && let Some(&g_idx) = candidate_idx.get(name.fq.as_str())
    {
        let next = match (state.get(&g_idx), resolve_const(value)) {
            (Some(Consensus::Disqualified), _) | (_, None) => Consensus::Disqualified,
            // First const assignment: adopt it.
            (None, Some(value)) => Consensus::Const(value),
            // A later assignment: keep `Const` only when it is structurally
            // identical to the first (the expected inlined-duplicate case). A
            // user-immutable global is assigned once in source, so a genuinely
            // different second value means the shape does not match our model —
            // disqualify rather than silently last-write-wins.
            (Some(Consensus::Const(prev)), Some(value)) => {
                if const_key(prev) == const_key(&value) {
                    Consensus::Const(value)
                } else {
                    Consensus::Disqualified
                }
            }
        };
        state.insert(g_idx, next);
        return;
    }
    instr.for_each_child(&mut |child| collect_assignments(child, candidate_idx, state));
}

/// Replace every `GlobalSet` to a promoted global with its dead-store-free
/// remainder, at any depth. When the assigned value is a builder `Seq`, its
/// binding locals may be read outside the store, so drop the whole `Seq` only
/// when they are provably dead elsewhere.
fn nop_promoted_assignments(
    instr: &mut WirInstr,
    promoted: &IndexSet<String>,
    read_counts: &IndexMap<String, u32>,
) {
    if let WirInstr::GlobalSet { name, value } = instr
        && promoted.contains(name.fq.as_str())
    {
        let replacement = drop_promoted_set(value, read_counts);
        *instr = replacement;
        return;
    }
    instr.for_each_boxed_child_mut(&mut |child| {
        nop_promoted_assignments(child, promoted, read_counts);
    });
}

/// The replacement for a promoted global's assignment. The resolved constant is
/// already installed in the global's eager `init`, so the store itself is
/// redundant. When its value is a builder `Seq` (`__b = …; __b`), dropping the
/// whole `Seq` would also drop its `LocalSet` bindings — unsound if a bound
/// local is read outside the store — so the `Seq` collapses to `Nop` only when
/// every bound local is dead elsewhere, otherwise to its binding prefix.
fn drop_promoted_set(value: &WirInstr, read_counts: &IndexMap<String, u32>) -> WirInstr {
    match value {
        WirInstr::RefAsNonNull(inner) => drop_promoted_set(inner, read_counts),
        WirInstr::Seq(items) => {
            let Some((_, binds)) = items.split_last() else {
                return WirInstr::Nop;
            };
            let mut internal: IndexMap<String, u32> = IndexMap::default();
            count_local_reads(value, &mut internal);
            let all_dead = binds.iter().all(|stmt| match stmt {
                WirInstr::LocalSet { name, .. } => {
                    read_counts.get(name).copied().unwrap_or(0)
                        <= internal.get(name).copied().unwrap_or(0)
                }
                _ => true,
            });
            if all_dead {
                WirInstr::Nop
            } else {
                WirInstr::Seq(binds.to_vec())
            }
        }
        _ => WirInstr::Nop,
    }
}

/// Resolve a `GlobalSet` value to a constant init expression.
///
/// Handles the shapes the extracted-then-optimized init takes: a direct
/// const value (scalar / `struct.new` / `array.new_*`), and a
/// side-effect-free `Seq` that binds const locals and returns one of them
/// (`__b = struct.new …; __b`) — the form an array literal produces via its
/// builder temp. A redundant `RefAsNonNull` wrapper is transparent. Returns
/// `None` for anything else (notably any non-const statement in a `Seq`,
/// which would make dropping the assignment unsound).
fn resolve_const(value: &WirInstr) -> Option<WirInstr> {
    resolve_with(value, &IndexMap::default())
}

fn resolve_with(value: &WirInstr, local_defs: &IndexMap<String, WirInstr>) -> Option<WirInstr> {
    match value {
        WirInstr::RefAsNonNull(inner) => resolve_with(inner, local_defs),
        WirInstr::LocalGet { name, .. } => local_defs.get(name.as_str()).cloned(),
        WirInstr::Seq(items) => {
            let (tail, init) = items.split_last()?;
            let mut defs = local_defs.clone();
            for stmt in init {
                let WirInstr::LocalSet { name, value } = stmt else {
                    return None; // a non-binding statement has side effects
                };
                let resolved = resolve_with(value, &defs)?;
                defs.insert(name.clone(), resolved);
            }
            resolve_with(tail, &defs)
        }
        _ => value.is_const_expressible().then(|| value.clone()),
    }
}
