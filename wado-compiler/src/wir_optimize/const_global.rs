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
//! already correctly lowered to WIR (variant representation, string data
//! segments, non-null field wrapping all baked in). For each such global
//! it moves the constant value into the global's eager `init`, marks the
//! global immutable, and drops the now-redundant `GlobalSet`. The dead
//! init temps, emptied `__initialize_module` body, and the
//! `__modules_initialized` guard are reclaimed by `elide_write_only_locals`
//! / `init_guard` / `dce` / `cleanup` in the same phase.
//!
//! It subsumes the former NIR `const_global_promotion` (scalar-only):
//! constness is decided once here, via [`WirInstr::is_const_expressible`].

use crate::hashmap::IndexMap;
use crate::wir::{WirInstr, WirPackage};

/// Per-candidate promotion state while scanning function bodies.
enum Candidate {
    /// Exactly one const assignment seen so far, at `(func, stmt)`.
    Promotable {
        func_idx: usize,
        stmt_idx: usize,
        value: WirInstr,
    },
    /// A non-const assignment, or more than one assignment: never eligible.
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

    // Locate each candidate's assignment. A user-immutable global is only
    // ever assigned at init time, so we expect exactly one top-level
    // `GlobalSet` in an `__initialize_module` body. More than one
    // assignment, or a non-const value, disqualifies it.
    let mut state: IndexMap<usize, Candidate> = IndexMap::default();
    for (func_idx, func) in module.functions.iter().enumerate() {
        let Some(body) = &func.body else { continue };
        for (stmt_idx, instr) in body.iter().enumerate() {
            let WirInstr::GlobalSet { name, value } = instr else {
                continue;
            };
            let Some(&g_idx) = candidate_idx.get(name.fq.as_str()) else {
                continue;
            };
            let resolved = resolve_const(value, &IndexMap::default());
            let next = match (state.contains_key(&g_idx), resolved) {
                (false, Some(value)) => Candidate::Promotable {
                    func_idx,
                    stmt_idx,
                    value,
                },
                _ => Candidate::Disqualified,
            };
            state.insert(g_idx, next);
        }
    }

    // Apply promotions, recording the `GlobalSet` statements to drop.
    let mut drops: IndexMap<usize, Vec<usize>> = IndexMap::default();
    for (g_idx, cand) in state {
        let Candidate::Promotable {
            func_idx,
            stmt_idx,
            value,
        } = cand
        else {
            continue;
        };
        let global = &mut module.globals[g_idx];
        global.init = value;
        global.mutable = false;
        // `lazy_init` and slot nullability are left as `register_globals`
        // set them: the slot stays nullable (a non-null const init is a
        // valid subtype) and codegen keeps narrowing reads with
        // `ref.as_non_null`, which is correct since the eager value is
        // non-null.
        drops.entry(func_idx).or_default().push(stmt_idx);
    }

    for (func_idx, stmt_indices) in drops {
        let Some(body) = &mut module.functions[func_idx].body else {
            continue;
        };
        for stmt_idx in stmt_indices {
            body[stmt_idx] = WirInstr::Nop;
        }
    }
}

/// Resolve a `GlobalSet` value to a constant init expression.
///
/// Handles three shapes that the extracted-then-optimized init takes:
/// a direct const value (scalar / `struct.new` / `array.new_*`), a
/// `LocalGet` of a const local defined in `local_defs`, and a
/// side-effect-free `Seq` that binds const locals and returns one of them
/// (`__b = struct.new …; __b`) — the form an array/string literal produces
/// via its builder temp. A redundant `RefAsNonNull` wrapper is transparent.
/// Returns `None` for anything else (notably any non-const statement in a
/// `Seq`, which would make dropping the assignment unsound).
fn resolve_const(value: &WirInstr, local_defs: &IndexMap<String, WirInstr>) -> Option<WirInstr> {
    match value {
        WirInstr::RefAsNonNull(inner) => resolve_const(inner, local_defs),
        WirInstr::LocalGet { name, .. } => local_defs.get(name.as_str()).cloned(),
        WirInstr::Seq(items) => {
            let (tail, init) = items.split_last()?;
            let mut defs = local_defs.clone();
            for stmt in init {
                let WirInstr::LocalSet { name, value } = stmt else {
                    return None; // a non-binding statement has side effects
                };
                let resolved = resolve_const(value, &defs)?;
                defs.insert(name.clone(), resolved);
            }
            resolve_const(tail, &defs)
        }
        _ => value.is_const_expressible().then(|| value.clone()),
    }
}
