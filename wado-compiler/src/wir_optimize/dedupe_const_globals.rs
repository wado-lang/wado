//! Deduplicate identical immutable constant globals, which `const_global`
//! promotes one apiece however many share a value. Such a global has no
//! observable identity under Wado's value semantics — reads copy out, `==` is
//! structural — so two with byte-identical inits and slot metadata are
//! interchangeable, and each group collapses to one canonical global.
//!
//! Identity *is* observable through `ref.eq` on two `&T` operands, so the pass
//! bails outright on a module containing any. Slot metadata joins the group key,
//! nullability deciding codegen's `ref.as_non_null`, and an exported global is
//! never merged away. Removal defers to `compact_globals`, a `Vec::retain` here
//! shifting its position-keyed index set.

use crate::hashmap::{IndexMap, IndexSet};
use crate::wir::{WirExportDesc, WirFuncId, WirInstr, WirPackage, WirType, WirTypeId};

/// Structural identity over the const-expressible `WirInstr` subset (see
/// [`WirInstr::is_const_expressible`]), distinguishing float payloads by their
/// bit pattern — two `NaN`s with different bits are *not* equal. A
/// `format!("{:?}", …)` key would print those identically and merge them (an
/// observable difference under `f*.to_bits`). `RefAsNonNull` is transparent (the
/// emitter drops it inside a const expression), so it never enters the key.
#[derive(PartialEq, Eq, Hash)]
pub(super) enum ConstKey {
    I32(i32),
    I64(i64),
    F32(u32),
    F64(u64),
    RefNull(String),
    RefFunc(WirFuncId),
    RefI31(Box<ConstKey>),
    StructNew(WirTypeId, Vec<ConstKey>),
    ArrayNewFixed(WirTypeId, Vec<ConstKey>),
    ArrayNewDefault(WirTypeId, Box<ConstKey>),
}

/// Build a [`ConstKey`] for a const-expressible instruction; `None` for
/// anything outside the subset.
pub(super) fn const_key(instr: &WirInstr) -> Option<ConstKey> {
    Some(match instr {
        WirInstr::I32Const(v) => ConstKey::I32(*v),
        WirInstr::I64Const(v) => ConstKey::I64(*v),
        WirInstr::F32Const(v) => ConstKey::F32(v.to_bits()),
        WirInstr::F64Const(v) => ConstKey::F64(v.to_bits()),
        WirInstr::RefNull { heap_type } => ConstKey::RefNull(format!("{heap_type:?}")),
        WirInstr::RefFunc { func_id } => ConstKey::RefFunc(func_id.clone()),
        WirInstr::RefI31(inner) => ConstKey::RefI31(Box::new(const_key(inner)?)),
        WirInstr::RefAsNonNull(inner) => const_key(inner)?,
        WirInstr::StructNew { type_id, fields } => {
            ConstKey::StructNew(type_id.clone(), const_keys(fields)?)
        }
        WirInstr::ArrayNewFixed { type_id, elements } => {
            ConstKey::ArrayNewFixed(type_id.clone(), const_keys(elements)?)
        }
        WirInstr::ArrayNewDefault { type_id, len } => {
            ConstKey::ArrayNewDefault(type_id.clone(), Box::new(const_key(len)?))
        }
        _ => return None,
    })
}

fn const_keys(instrs: &[WirInstr]) -> Option<Vec<ConstKey>> {
    instrs.iter().map(const_key).collect()
}

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
    // The type carries no float payload, so its `Debug` string is a safe key
    // component; the init uses the bit-exact [`ConstKey`].
    type Key = (bool, bool, bool, String, ConstKey);
    let mut groups: IndexMap<Key, Vec<usize>> = IndexMap::default();
    for (i, g) in module.globals.iter().enumerate() {
        if g.mutable || exported.contains(g.name.fq.as_str()) || !is_reference_type(&g.ty) {
            continue;
        }
        let Some(init_key) = const_key(&g.init) else {
            continue;
        };
        let key = (
            g.lazy_init,
            g.wado_mutable,
            matches!(
                &g.ty,
                WirType::Ref { nullable: true, .. } | WirType::AbstractRef { nullable: true, .. }
            ),
            format!("{:?}", g.ty),
            init_key,
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
        || module
            .data
            .iter()
            .any(|d| d.offset.as_ref().is_some_and(has))
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
