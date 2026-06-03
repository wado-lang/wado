//! Deduplicate identical immutable constant globals.
//!
//! `const_global` promotes each constant aggregate global to an eager,
//! immutable Wasm constant — but identical const aggregates (two `[10, 20, 30]`
//! hoisted by `const_object_globalization`, two equal user const globals, two
//! `"hi"` short-string globals, duplicate Gale grammar tables, …) each get
//! their own global. An immutable, const-expressible, value-typed global has no
//! observable identity under Wado's value semantics — reads copy out and `==`
//! is structural — so two with byte-identical inits and identical slot metadata
//! are interchangeable. This pass merges each such group to one canonical
//! global, rewrites every reference, and marks the rest dead.
//!
//! ## Soundness
//!
//! - **Reference identity.** The one way to observe that two equal-value
//!   references are distinct objects is `==` on two `&T` operands, which the
//!   elaborator lowers to `ref.eq` (`reify.rs`; `GlobalGet` of a value-typed
//!   global is never itself a `ref.eq` operand, but `&GLOBAL` is). Tracing
//!   which globals can reach a `ref.eq` through locals is flow-sensitive; since
//!   `ref.eq` is absent from ordinary programs (it needs an explicit
//!   `&a == &b`), the pass simply **bails entirely if the module contains any
//!   `ref.eq`**. Conservative and cheap.
//! - **Slot metadata.** Codegen reads `lazy_init` / nullability *by name*
//!   (`is_lazy_init_global` decides whether a `global.get` is narrowed with
//!   `ref.as_non_null`), so two globals are merged only when their `lazy_init`,
//!   `is`-nullable type, and `wado_mutable` all match — folded into the group
//!   key alongside the structural `(ty, init)`.
//! - **Exports.** An exported global is never merged away (its export names it).
//!
//! ## Removal
//!
//! Globals are removed by recording their indices in `dead_global_indices` —
//! the same channel `init_guard` and the `#![wasm_module]` extractor use — so
//! phase-8 `compact_globals` drops them in one pass. Removing them here with
//! `Vec::retain` would shift indices out from under that position-keyed set and
//! corrupt it; deferring keeps every index in one space. All references to a
//! merged-away global (`GlobalGet`/`GlobalSet` in function bodies, global
//! inits, and data/element segment offsets) are rewritten to the canonical
//! first, so the dead globals are unreferenced when `compact_globals` runs.
//!
//! Runs in phase 7, right after `promote_const_global_inits` makes the
//! candidates immutable.

use crate::hashmap::{IndexMap, IndexSet};
use crate::wir::{WirExportDesc, WirInstr, WirPackage, WirType};

pub(super) fn dedupe_const_globals(module: &mut WirPackage) {
    // Bail if any `ref.eq` exists: merging two equal-value references could flip
    // a reference-identity comparison, and tracing reachability through locals
    // is not worth it for an instruction ordinary programs never emit.
    if module_has_ref_eq(module) {
        return;
    }

    // Globals named by an export must not be merged away.
    let exported: IndexSet<String> = module
        .exports
        .iter()
        .filter_map(|e| match &e.desc {
            WirExportDesc::Global { name } => Some(name.fq.clone()),
            _ => None,
        })
        .collect();

    // Group eligible globals by a structural key of (slot metadata, type, init).
    // Only immutable, value(reference)-typed, const-expressible, non-exported
    // globals qualify; metadata is in the key so reads keep identical
    // nullability / lazy-init narrowing after the merge.
    type Key = (bool, bool, bool, String, String);
    let mut groups: IndexMap<Key, Vec<usize>> = IndexMap::default();
    for (i, g) in module.globals.iter().enumerate() {
        if g.mutable
            || exported.contains(g.name.fq.as_str())
            || !is_reference_type(&g.ty)
            || !g.init.is_const_expressible()
        {
            continue;
        }
        let key = (
            g.lazy_init,
            g.wado_mutable,
            matches!(&g.ty, WirType::Ref { nullable: true, .. } | WirType::AbstractRef { nullable: true, .. }),
            format!("{:?}", g.ty),
            format!("{:?}", g.init),
        );
        groups.entry(key).or_default().push(i);
    }

    // For each group of ≥2, keep the first as canonical and map the rest onto
    // it. `rename` maps a merged-away global's name to its canonical name.
    let mut rename: IndexMap<String, String> = IndexMap::default();
    let mut dead_indices: Vec<u32> = Vec::new();
    for indices in groups.values() {
        if indices.len() < 2 {
            continue;
        }
        let canonical = module.globals[indices[0]].name.fq.clone();
        for &dup in &indices[1..] {
            rename.insert(module.globals[dup].name.fq.clone(), canonical.clone());
            dead_indices.push(u32::try_from(dup).expect("global index fits u32"));
        }
    }
    if rename.is_empty() {
        return;
    }

    // Rewrite every reference to a merged-away global to its canonical name,
    // across all instruction channels.
    for func in &mut module.functions {
        if let Some(body) = &mut func.body {
            for instr in body.iter_mut() {
                rewrite(instr, &rename);
            }
        }
    }
    for g in &mut module.globals {
        rewrite(&mut g.init, &rename);
    }
    for d in &mut module.data {
        if let Some(offset) = &mut d.offset {
            rewrite(offset, &rename);
        }
    }
    for e in &mut module.elements {
        rewrite(&mut e.offset, &rename);
    }

    // Mark the merged-away globals dead; phase-8 `compact_globals` removes them
    // together with any other dead globals, keeping indices consistent.
    module.dead_global_indices.extend(dead_indices);
}

fn is_reference_type(ty: &WirType) -> bool {
    matches!(ty, WirType::Ref { .. } | WirType::AbstractRef { .. })
}

/// True if any instruction anywhere in the module is a `RefEq`.
fn module_has_ref_eq(module: &WirPackage) -> bool {
    fn has(instr: &WirInstr) -> bool {
        if matches!(instr, WirInstr::RefEq(..)) {
            return true;
        }
        let mut found = false;
        instr.for_each_child(&mut |child| found |= has(child));
        found
    }
    module
        .functions
        .iter()
        .filter_map(|f| f.body.as_ref())
        .flatten()
        .any(has)
        || module.globals.iter().any(|g| has(&g.init))
        || module.data.iter().any(|d| d.offset.as_ref().is_some_and(has))
        || module.elements.iter().any(|e| has(&e.offset))
}

/// Rewrite `GlobalGet`/`GlobalSet` names per `rename`, recursively.
fn rewrite(instr: &mut WirInstr, rename: &IndexMap<String, String>) {
    match instr {
        WirInstr::GlobalGet { name, .. } | WirInstr::GlobalSet { name, .. } => {
            if let Some(canonical) = rename.get(name.fq.as_str()) {
                name.fq = canonical.clone();
            }
        }
        _ => {}
    }
    instr.for_each_boxed_child_mut(&mut |child| rewrite(child, rename));
}
