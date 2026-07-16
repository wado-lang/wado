//! Struct field constant forwarding for WIR.
//!
//! Per-function pass that propagates known constant field values through `StructGet`,
//! folds constant comparisons, and eliminates dead branches.

use crate::hashmap::{IndexMap, IndexSet};
use crate::wir::{WirFunction, WirInstr, WirPackage, WirTypeDef, WirTypeId};

use super::util::collect_local_gets_deep;

pub(super) fn forward_struct_field_constants(module: &mut WirPackage) {
    let types = &module.types;
    let defined_func_base = module.defined_func_base;
    for func_idx in 0..module.functions.len() {
        let Some(body) = module.functions[func_idx].body.take() else {
            continue;
        };
        // Locals connected by plain local-to-local copies share one GC object;
        // mutations and aliasing must apply to the whole group.
        let copy_groups = collect_copy_groups(&body);
        // Collect locals whose references escape. Field forwarding is unsafe
        // for these locals because their fields can be modified through aliases.
        // Uses stores info: locals passed to functions without `stores` for that
        // parameter are NOT marked as aliased.
        let mut aliased = collect_aliased_locals(&body, &module.functions, defined_func_base);
        widen_aliased_across_copy_groups(&mut aliased, &copy_groups);
        // Locals assigned exactly once. `local_const` folds a `LocalGet` to its
        // bound constant, which is only sound for single-assignment locals: a
        // reassignment elsewhere — in particular a `local.tee` or a `LocalSet`
        // nested in a subexpression, which the per-instruction knowledge update
        // does not observe — would otherwise leave a later read folding to a
        // stale value. (The `local_const` docstring already assumed single
        // assignment; this enforces it.)
        let single_assigned = single_assigned_locals(&body);
        let mut body = body;
        let mut changed = true;
        while changed {
            let mut known = FieldKnowledge::new(types, &aliased, &single_assigned, &copy_groups);
            changed = forward_fields_in_body(&mut body, &mut known);
        }
        module.functions[func_idx].body = Some(body);
    }
}

/// Locals assigned exactly once across the whole body, counting every
/// `LocalSet`/`LocalTee`/`MultiValueLocalBind` def including those nested
/// in subexpressions.
fn single_assigned_locals(body: &[WirInstr]) -> IndexSet<String> {
    let mut counts: IndexMap<String, u32> = IndexMap::default();
    for instr in body {
        count_assignments_in_instr(instr, &mut counts);
    }
    counts
        .into_iter()
        .filter(|(_, c)| *c == 1)
        .map(|(name, _)| name)
        .collect()
}

fn count_assignments_in_instr(instr: &WirInstr, counts: &mut IndexMap<String, u32>) {
    match instr {
        WirInstr::LocalSet { name, .. } | WirInstr::LocalTee { name, .. } => {
            *counts.entry(name.clone()).or_insert(0) += 1;
        }
        WirInstr::MultiValueLocalBind { locals, .. } => {
            for name in locals.iter().flatten() {
                *counts.entry(name.clone()).or_insert(0) += 1;
            }
        }
        _ => {}
    }
    instr.for_each_child(&mut |child| count_assignments_in_instr(child, counts));
}

/// The possible copied-from locals of a plain local-to-local copy value: a
/// direct `LocalGet`, every break-value local of a `Block` (each exit may
/// hand out a different local's object), or a `Seq` tail `LocalGet`.
fn copy_sources(value: &WirInstr) -> Vec<String> {
    match value {
        WirInstr::LocalGet { name, .. } => vec![name.clone()],
        WirInstr::Block { body, .. } => block_exit_locals(body),
        WirInstr::Seq(body) => extract_seq_result_local(body).into_iter().collect(),
        _ => Vec::new(),
    }
}

/// Locals in break-value position of a value block: any `LocalGet`
/// immediately followed by a `Br` targeting the block's own label.
fn block_exit_locals(body: &[WirInstr]) -> Vec<String> {
    let mut out = Vec::new();
    collect_exit_locals_in_body(body, 0, &mut out);
    out
}

fn collect_exit_locals_in_body(body: &[WirInstr], nesting: u32, out: &mut Vec<String>) {
    for pair in body.windows(2) {
        if let [WirInstr::LocalGet { name, .. }, WirInstr::Br { depth }] = pair
            && *depth == nesting
        {
            out.push(name.clone());
        }
    }
    for instr in body {
        collect_exit_locals_in_instr(instr, nesting, out);
    }
}

fn collect_exit_locals_in_instr(instr: &WirInstr, nesting: u32, out: &mut Vec<String>) {
    match instr {
        WirInstr::Block { body, .. } | WirInstr::Loop { body, .. } => {
            collect_exit_locals_in_body(body, nesting + 1, out);
        }
        WirInstr::If {
            condition,
            then_body,
            else_body,
            ..
        } => {
            collect_exit_locals_in_instr(condition, nesting, out);
            collect_exit_locals_in_body(then_body, nesting + 1, out);
            if let Some(eb) = else_body {
                collect_exit_locals_in_body(eb, nesting + 1, out);
            }
        }
        WirInstr::Seq(body) => collect_exit_locals_in_body(body, nesting, out),
        _ => {
            instr.for_each_child(&mut |child| {
                collect_exit_locals_in_instr(child, nesting, out);
            });
        }
    }
}

/// Group locals connected by plain local-to-local copies (union-find over
/// copy edges, flow-insensitive). A bare copy of a GC value shares the object
/// — Wado value semantics insert explicit `value_copy` calls where a deep
/// copy is required — so a mutation observed through one member is observable
/// through every member. Used to widen invalidation and alias marking.
///
/// Returns `local name → group id`; locals with no copy edge are absent.
fn collect_copy_groups(body: &[WirInstr]) -> IndexMap<String, u32> {
    let mut edges: Vec<(String, String)> = Vec::new();
    for instr in body {
        collect_copy_edges(instr, &mut edges);
    }

    let mut index_of: IndexMap<String, u32> = IndexMap::default();
    let mut parent: Vec<u32> = Vec::new();
    let mut intern = |name: &str, parent: &mut Vec<u32>| -> u32 {
        if let Some(i) = index_of.get(name) {
            return *i;
        }
        let i = u32::try_from(parent.len()).expect("local count fits u32");
        index_of.insert(name.to_string(), i);
        parent.push(i);
        i
    };
    fn find(parent: &mut [u32], mut i: u32) -> u32 {
        while parent[i as usize] != i {
            parent[i as usize] = parent[parent[i as usize] as usize];
            i = parent[i as usize];
        }
        i
    }
    for (to, from) in &edges {
        let a = intern(to, &mut parent);
        let b = intern(from, &mut parent);
        let (ra, rb) = (find(&mut parent, a), find(&mut parent, b));
        parent[ra as usize] = rb;
    }

    let mut groups = IndexMap::default();
    for (name, i) in &index_of {
        groups.insert(name.clone(), find(&mut parent, *i));
    }
    groups
}

fn collect_copy_edges(instr: &WirInstr, edges: &mut Vec<(String, String)>) {
    if let WirInstr::LocalSet { name, value } | WirInstr::LocalTee { name, value } = instr {
        for source in copy_sources(value) {
            edges.push((name.clone(), source));
        }
    }
    instr.for_each_child(&mut |child| collect_copy_edges(child, edges));
}

/// Aliasing infects a whole copy group: every member shares the object.
fn widen_aliased_across_copy_groups(
    aliased: &mut IndexSet<String>,
    copy_groups: &IndexMap<String, u32>,
) {
    let dirty_groups: IndexSet<u32> = copy_groups
        .iter()
        .filter(|(name, _)| aliased.contains(name.as_str()))
        .map(|(_, group)| *group)
        .collect();
    for (name, group) in copy_groups {
        if dirty_groups.contains(group) {
            aliased.insert(name.clone());
        }
    }
}

/// Collect locals whose references escape (address taken, embedded in structs,
/// or passed to function calls that declare `stores` for that parameter).
/// Locals passed to functions without `stores` are NOT aliased — the callee
/// cannot retain the reference beyond the call.
fn collect_aliased_locals(
    body: &[WirInstr],
    functions: &[WirFunction],
    defined_func_base: u32,
) -> IndexSet<String> {
    let mut aliased = IndexSet::default();
    for instr in body {
        collect_aliased_in_instr(instr, &mut aliased, functions, defined_func_base, false);
    }
    aliased
}

/// Recursively collect aliased locals.
///
/// `in_non_stores_arg`: when true, we are inside a Call argument whose callee
/// does not declare `stores` for this parameter position. In this context,
/// `RefAsNonNull(LocalGet)` and bare `LocalGet` do not create persistent aliases.
///
/// A plain `LocalSet(b, LocalGet a)` copy does NOT alias either side: the two
/// names share the object (value semantics emit explicit `value_copy` calls
/// where deep copies are required), but the copy itself creates no untracked
/// mutation channel — `copy_field_knowledge` transfers the facts and every
/// mutation channel (`StructSet`, call argument, escaped reference)
/// invalidates the whole copy group.
fn collect_aliased_in_instr(
    instr: &WirInstr,
    aliased: &mut IndexSet<String>,
    functions: &[WirFunction],
    defined_func_base: u32,
    in_non_stores_arg: bool,
) {
    match instr {
        // Direct function calls: check stores for each parameter.
        WirInstr::Call { func_id, args } => {
            // `WirFuncId` carries the absolute Wasm function index; defined
            // functions start at `defined_func_base`. Imports sit below the
            // base and have no `stores` metadata — unknown, so conservative.
            let callee = func_id
                .index()
                .checked_sub(defined_func_base)
                .and_then(|i| functions.get(i as usize));
            for (i, arg) in args.iter().enumerate() {
                let stores_param = match callee {
                    Some(f) => match f.param_names.get(i) {
                        Some(param) => f.stores.iter().any(|s| s == param),
                        None => true,
                    },
                    None => true,
                };
                if stores_param {
                    // Callee may store this reference — mark all locals as aliased.
                    collect_local_gets_deep(arg, aliased);
                }
                // Recurse into sub-expressions (nested calls get their own analysis).
                collect_aliased_in_instr(arg, aliased, functions, defined_func_base, !stores_param);
            }
            return; // Skip default for_each_child — args handled above.
        }
        // Indirect calls: conservative (unknown callee).
        WirInstr::CallRef { func_ref, args, .. } => {
            for arg in args {
                collect_local_gets_deep(arg, aliased);
                collect_aliased_in_instr(arg, aliased, functions, defined_func_base, false);
            }
            collect_aliased_in_instr(func_ref, aliased, functions, defined_func_base, false);
            return;
        }
        WirInstr::CallIndirect { index, args, .. } => {
            for arg in args {
                collect_local_gets_deep(arg, aliased);
                collect_aliased_in_instr(arg, aliased, functions, defined_func_base, false);
            }
            collect_aliased_in_instr(index, aliased, functions, defined_func_base, false);
            return;
        }
        // RefAsNonNull of a LocalGet: address taken — but suppress if inside
        // a non-stores call argument (the reference doesn't persist).
        WirInstr::RefAsNonNull(inner) => {
            if !in_non_stores_arg && let WirInstr::LocalGet { name, .. } = inner.as_ref() {
                aliased.insert(name.clone());
            }
        }
        _ => {}
    }
    // Recurse into children, propagating the suppression context.
    instr.for_each_child(&mut |child| {
        collect_aliased_in_instr(child, aliased, functions, defined_func_base, in_non_stores_arg);
    });
}

/// Known constant field values for locals.
/// Maps `(local_name, field_name)` → constant `WirInstr`.
struct FieldKnowledge<'a> {
    /// Known constant field values: `(local_name, field_name)` → constant value
    fields: IndexMap<(String, String), WirInstr>,
    /// Single-assignment locals bound to a constant: `local_name` → const instr.
    /// A `LocalGet` of one is replaced by the constant, so a materialised
    /// `let _av = obj.f` whose field folds to a constant (`niri_*_preserves_fields`)
    /// propagates into `1 >= _av` and the dead branch prunes. Only locals in
    /// `single_assigned` are recorded here.
    local_const: IndexMap<String, WirInstr>,
    /// Type definitions for resolving field names by index
    types: &'a [WirTypeDef],
    /// Locals that are aliased and unsafe for field forwarding
    aliased: &'a IndexSet<String>,
    /// Locals assigned exactly once — the only ones eligible for `local_const`.
    single_assigned: &'a IndexSet<String>,
    /// Copy groups (see `collect_copy_groups`) — mutation invalidation widens
    /// across them.
    copy_groups: &'a IndexMap<String, u32>,
}

impl<'a> FieldKnowledge<'a> {
    fn new(
        types: &'a [WirTypeDef],
        aliased: &'a IndexSet<String>,
        single_assigned: &'a IndexSet<String>,
        copy_groups: &'a IndexMap<String, u32>,
    ) -> Self {
        Self {
            fields: IndexMap::default(),
            local_const: IndexMap::default(),
            types,
            aliased,
            single_assigned,
            copy_groups,
        }
    }

    /// Clone the mutable knowledge for a conditionally executed region.
    fn fork(&self) -> FieldKnowledge<'a> {
        FieldKnowledge {
            fields: self.fields.clone(),
            local_const: self.local_const.clone(),
            types: self.types,
            aliased: self.aliased,
            single_assigned: self.single_assigned,
            copy_groups: self.copy_groups,
        }
    }

    /// Record known fields from a `StructNew` assigned to `local_name`.
    /// Records both constant values and `LocalGet` references.
    /// Skips aliased locals (their fields may be modified through references).
    fn record_struct_new(&mut self, local_name: &str, type_id: WirTypeId, fields: &[WirInstr]) {
        if self.aliased.contains(local_name) {
            return;
        }
        let Some(WirTypeDef::Struct(st)) = self.types.get(type_id.index() as usize) else {
            return;
        };
        for (i, field_def) in st.fields.iter().enumerate() {
            let Some(field_instr) = fields.get(i) else {
                continue;
            };
            if !is_forwardable(field_instr) {
                continue;
            }
            // A `LocalGet` field value means "equals the local's CURRENT
            // value". A def of that local in a later field expression makes
            // the recorded read stale — skip it.
            if let WirInstr::LocalGet { name: src, .. } = field_instr
                && fields[i + 1..].iter().any(|later| contains_def_of(later, src))
            {
                continue;
            }
            self.fields.insert(
                (local_name.to_string(), field_def.name.clone()),
                field_instr.clone(),
            );
        }
    }

    /// Look up a known constant for `local_name.field_name`.
    fn get(&self, local_name: &str, field_name: &str) -> Option<&WirInstr> {
        self.fields
            .get(&(local_name.to_string(), field_name.to_string()))
    }

    /// Invalidate all known fields for a local (on reassignment).
    /// Also invalidates entries whose stored value is a `LocalGet` referencing
    /// the reassigned local, since that value is no longer valid.
    fn invalidate_local(&mut self, local_name: &str) {
        self.local_const.swap_remove(local_name);
        self.fields.retain(|(name, _), val| {
            if name == local_name {
                return false;
            }
            // If the stored value references the reassigned local, invalidate it
            if let WirInstr::LocalGet { name: source, .. } = val
                && source == local_name
            {
                return false;
            }
            true
        });
    }

    /// Invalidate a specific field for a local (on `StructSet`).
    fn invalidate_field(&mut self, local_name: &str, field_name: &str) {
        self.fields
            .swap_remove(&(local_name.to_string(), field_name.to_string()));
    }

    /// Locals sharing `name`'s object through plain copies (excluding `name`).
    fn copy_group_members(&self, name: &str) -> Vec<String> {
        let Some(group) = self.copy_groups.get(name) else {
            return Vec::new();
        };
        self.copy_groups
            .iter()
            .filter(|(member, g)| *g == group && member.as_str() != name)
            .map(|(member, _)| member.clone())
            .collect()
    }

    /// Invalidate a local whose object may have been mutated (call argument,
    /// escaped reference). The mutation is visible through every copy-group
    /// member, so the whole group is invalidated.
    fn invalidate_mutated_local(&mut self, name: &str) {
        self.invalidate_local(name);
        for member in self.copy_group_members(name) {
            self.invalidate_local(&member);
        }
    }

    /// Invalidate a field mutated through `name` (`StructSet`), across the
    /// whole copy group.
    fn invalidate_mutated_field(&mut self, name: &str, field_name: &str) {
        self.invalidate_field(name, field_name);
        for member in self.copy_group_members(name) {
            self.invalidate_field(&member, field_name);
        }
    }
}

/// True if `instr`'s subtree contains a def (`LocalSet`/`LocalTee`/
/// `MultiValueLocalBind`) of `local`.
fn contains_def_of(instr: &WirInstr, local: &str) -> bool {
    match instr {
        WirInstr::LocalSet { name, .. } | WirInstr::LocalTee { name, .. } if name == local => {
            return true;
        }
        WirInstr::MultiValueLocalBind { locals, .. }
            if locals.iter().flatten().any(|n| n == local) =>
        {
            return true;
        }
        _ => {}
    }
    let mut found = false;
    instr.for_each_child(&mut |child| found = found || contains_def_of(child, local));
    found
}

/// Check if a WIR instruction is a constant value.
fn is_wir_constant(instr: &WirInstr) -> bool {
    matches!(
        instr,
        WirInstr::I32Const(_)
            | WirInstr::I64Const(_)
            | WirInstr::F32Const(_)
            | WirInstr::F64Const(_)
    )
}

/// Check if a WIR instruction is forwardable through struct fields.
/// Includes constants and `LocalGet` (variable references).
fn is_forwardable(instr: &WirInstr) -> bool {
    is_wir_constant(instr) || matches!(instr, WirInstr::LocalGet { .. })
}

/// Process a body (list of instructions), forwarding known constants.
/// Returns true if any changes were made.
///
/// Once a statement can branch out of the enclosing block, the remainder of
/// the body only executes on the fall-through path: its facts (and the exit
/// statement's own bindings) are processed in a fork of the state, and only
/// invalidations merge back — mirroring the `If` treatment. Facts recorded
/// before the first possible exit hold on every path and flow through.
fn forward_fields_in_body(body: &mut [WirInstr], known: &mut FieldKnowledge<'_>) -> bool {
    let mut changed = false;
    for i in 0..body.len() {
        changed |= forward_fields_in_instr(&mut body[i], known);
        if may_exit_enclosing_block(&body[i]) {
            let mut tail_known = known.fork();
            update_knowledge_from_instr(&body[i], &mut tail_known);
            for j in (i + 1)..body.len() {
                changed |= forward_fields_in_instr(&mut body[j], &mut tail_known);
                update_knowledge_from_instr(&body[j], &mut tail_known);
            }
            for stmt in &body[i..] {
                invalidate_effects_in_instr(stmt, known, InvalidationScope::Merge);
            }
            return changed;
        }
        update_knowledge_from_instr(&body[i], known);
    }
    changed
}

/// True if executing `instr` may branch out of the enclosing block: it
/// contains a `Br`/`BrIf`/`BrTable` whose target is the block's own label or
/// any label beyond it. `Return`/`Unreachable` do not count — they never
/// resume after the block, so they cannot carry stale facts there.
fn may_exit_enclosing_block(instr: &WirInstr) -> bool {
    branches_at_or_beyond(instr, 0)
}

fn branches_at_or_beyond(instr: &WirInstr, label_depth: u32) -> bool {
    match instr {
        WirInstr::Br { depth } => *depth >= label_depth,
        WirInstr::BrIf { depth, condition } => {
            *depth >= label_depth || branches_at_or_beyond(condition, label_depth)
        }
        WirInstr::BrTable {
            index,
            targets,
            default,
        } => {
            targets
                .iter()
                .chain(std::iter::once(default))
                .any(|d| *d >= label_depth)
                || branches_at_or_beyond(index, label_depth)
        }
        WirInstr::Block { body, .. } | WirInstr::Loop { body, .. } => body
            .iter()
            .any(|child| branches_at_or_beyond(child, label_depth + 1)),
        WirInstr::If {
            condition,
            then_body,
            else_body,
            ..
        } => {
            branches_at_or_beyond(condition, label_depth)
                || then_body
                    .iter()
                    .any(|child| branches_at_or_beyond(child, label_depth + 1))
                || else_body
                    .iter()
                    .flatten()
                    .any(|child| branches_at_or_beyond(child, label_depth + 1))
        }
        WirInstr::Seq(body) => body
            .iter()
            .any(|child| branches_at_or_beyond(child, label_depth)),
        _ => {
            let mut found = false;
            instr.for_each_child(&mut |child| {
                found = found || branches_at_or_beyond(child, label_depth);
            });
            found
        }
    }
}

/// Update field knowledge from a statement (after its rewrite): first
/// invalidate every mutation effect in the statement, then record the
/// positive facts of the recognized top-level shapes.
fn update_knowledge_from_instr(instr: &WirInstr, known: &mut FieldKnowledge<'_>) {
    invalidate_effects_in_instr(instr, known, InvalidationScope::Statement);
    record_knowledge_from_instr(instr, known);
}

fn record_knowledge_from_instr(instr: &WirInstr, known: &mut FieldKnowledge<'_>) {
    let WirInstr::LocalSet { name, value } = instr else {
        return;
    };
    match value.as_ref() {
        // Direct StructNew: record known fields
        WirInstr::StructNew { type_id, fields } => {
            known.record_struct_new(name, type_id.clone(), fields);
        }
        // Block whose result is a LocalGet: copy knowledge from that local
        // Block whose result is a StructNew: record its fields
        WirInstr::Block { body, .. } => {
            if let Some(source_name) = extract_block_result_local(body) {
                copy_field_knowledge(known, &source_name, name);
            } else if let Some((type_id, fields)) = extract_block_result_struct_new(body) {
                known.record_struct_new(name, type_id, &fields);
            }
        }
        // Seq whose tail is a LocalGet: copy knowledge from that local.
        // Seq whose tail is a StructNew: record its fields.
        //
        // This case appears after `branch_prune` flattens a labeled
        // value block (`label: block -> T { …; break label: V; }`)
        // into a plain stmt list — the NIR `Block` translates to a
        // WIR `Seq`, and the tail expression is the result.
        WirInstr::Seq(body) => {
            if let Some(source_name) = extract_seq_result_local(body) {
                copy_field_knowledge(known, &source_name, name);
            } else if let Some((type_id, fields)) = extract_seq_result_struct_new(body) {
                known.record_struct_new(name, type_id, &fields);
            }
        }
        // LocalGet: copy knowledge from source local
        WirInstr::LocalGet { name: source, .. } => {
            copy_field_knowledge(known, source, name);
        }
        // A constant binding `local = <const>`: record it so a later
        // `LocalGet local` folds to the constant. Skip aliased locals
        // (their value can be overwritten through a retained reference).
        v if is_wir_constant(v)
            && !known.aliased.contains(name)
            && known.single_assigned.contains(name) =>
        {
            known.local_const.insert(name.clone(), v.clone());
        }
        _ => {}
    }
}

#[derive(Clone, Copy, PartialEq)]
enum InvalidationScope {
    /// Straight-line statement walk: skip `Block`/`Seq`/`Loop`/`If` bodies —
    /// the forward walk just processed those with their own knowledge updates
    /// (recording where unconditional, invalidating where conditional), so
    /// re-invalidating would erase facts they soundly recorded. Effects in
    /// plain expression positions (nested tees, calls, conditions) are the
    /// ones no other walk observes.
    Statement,
    /// Conditionally executed body at a merge point (`If`/`Loop` join, the
    /// remainder of a block after a possible early exit): every effect in
    /// the whole subtree invalidates.
    Merge,
}

/// The single invalidator shared by the straight-line and merge paths:
/// invalidates defs (`LocalSet`/`LocalTee`/`MultiValueLocalBind`), field
/// mutations (`StructSet`), and mutations through call arguments or escaped
/// references. Mutation channels invalidate across copy groups.
fn invalidate_effects_in_instr(
    instr: &WirInstr,
    known: &mut FieldKnowledge<'_>,
    scope: InvalidationScope,
) {
    match instr {
        WirInstr::LocalSet { name, .. } | WirInstr::LocalTee { name, .. } => {
            known.invalidate_local(name);
        }
        WirInstr::MultiValueLocalBind { locals, .. } => {
            for name in locals.iter().flatten() {
                known.invalidate_local(name);
            }
        }
        WirInstr::StructSet {
            expr, field_name, ..
        } => {
            if let WirInstr::LocalGet { name, .. } = expr.as_ref() {
                known.invalidate_mutated_field(name, field_name);
            }
        }
        // A reference to a local escaping anywhere — call argument, stored
        // into a struct — is a mutation channel for the pointee.
        WirInstr::RefAsNonNull(inner) => {
            if let WirInstr::LocalGet { name, .. } = inner.as_ref() {
                known.invalidate_mutated_local(name);
            }
        }
        // Direct calls: a reference argument can be mutated during the call
        // even by a callee without `stores`, so every top-level `LocalGet`
        // argument invalidates. `RefAsNonNull(LocalGet)` nested in an
        // argument (a `&mut` embedded in a literal) is caught by the arm
        // above during recursion.
        WirInstr::Call { args, .. } => {
            for arg in args {
                if let WirInstr::LocalGet { name, .. } = arg {
                    known.invalidate_mutated_local(name);
                }
            }
        }
        // Unknown callees: conservatively treat every local reachable from
        // the arguments as mutated.
        WirInstr::CallRef { args, .. } | WirInstr::CallIndirect { args, .. } => {
            let mut names = IndexSet::default();
            for arg in args {
                collect_local_gets_deep(arg, &mut names);
            }
            for name in &names {
                known.invalidate_mutated_local(name);
            }
        }
        WirInstr::Block { .. } | WirInstr::Seq(_) | WirInstr::Loop { .. } => {
            if scope == InvalidationScope::Statement {
                return;
            }
        }
        WirInstr::If { condition, .. } => {
            if scope == InvalidationScope::Statement {
                invalidate_effects_in_instr(condition, known, scope);
                return;
            }
        }
        _ => {}
    }
    instr.for_each_child(&mut |child| invalidate_effects_in_instr(child, known, scope));
}

fn invalidate_merged_body(body: &[WirInstr], known: &mut FieldKnowledge<'_>) {
    for instr in body {
        invalidate_effects_in_instr(instr, known, InvalidationScope::Merge);
    }
}

/// Process a single instruction, recursively forwarding constants.
/// Returns true if any changes were made.
fn forward_fields_in_instr(instr: &mut WirInstr, known: &mut FieldKnowledge<'_>) -> bool {
    let mut changed = false;

    match instr {
        // Recurse into block bodies
        WirInstr::Block { body, .. } | WirInstr::Seq(body) => {
            changed |= forward_fields_in_body(body, known);
        }
        WirInstr::Loop { body, .. } => {
            // Conservatively invalidate all knowledge for loops
            // (locals could be modified on back-edges)
            let mut loop_known = FieldKnowledge::new(
                known.types,
                known.aliased,
                known.single_assigned,
                known.copy_groups,
            );
            changed |= forward_fields_in_body(body, &mut loop_known);
            // Invalidate outer knowledge for locals modified inside the loop
            invalidate_merged_body(body, known);
        }
        WirInstr::If {
            condition,
            then_body,
            else_body,
            ..
        } => {
            // Forward in the condition, then apply its effects (nested tees,
            // calls) before the branch states fork.
            changed |= forward_fields_in_instr(condition, known);
            invalidate_effects_in_instr(condition, known, InvalidationScope::Statement);

            // Forward into branches with cloned knowledge
            let mut then_known = known.fork();
            changed |= forward_fields_in_body(then_body, &mut then_known);
            if let Some(eb) = else_body {
                let mut else_known = known.fork();
                changed |= forward_fields_in_body(eb, &mut else_known);
            }
            // Conservatively invalidate locals modified in branches
            invalidate_merged_body(then_body, known);
            if let Some(eb) = else_body {
                invalidate_merged_body(eb, known);
            }
        }
        // A read of a constant-bound single-assignment local folds to the constant.
        WirInstr::LocalGet { name, .. } if known.local_const.contains_key(name.as_str()) => {
            let c = known.local_const.get(name.as_str()).unwrap().clone();
            *instr = c;
            changed = true;
        }
        _ => {
            // For other instructions, try to forward StructGet(LocalGet(x), field)
            changed |= try_forward_struct_gets(instr, known);
            // Recurse into children
            instr.for_each_boxed_child_mut(&mut |child| {
                changed |= forward_fields_in_instr(child, known);
            });
        }
    }

    changed
}

/// Try to replace `StructGet(LocalGet(x), field)` with a known constant.
fn try_forward_struct_gets(instr: &mut WirInstr, known: &FieldKnowledge<'_>) -> bool {
    if let WirInstr::StructGet {
        field_name, expr, ..
    } = instr
        && let WirInstr::LocalGet { name, .. } = expr.as_ref()
        && let Some(const_val) = known.get(name, field_name)
    {
        *instr = const_val.clone();
        return true;
    }
    false
}

/// Return the slice with any trailing `Unreachable` instructions removed.
fn skip_trailing_unreachable(body: &[WirInstr]) -> &[WirInstr] {
    let mut end = body.len();
    while end > 0 && matches!(body[end - 1], WirInstr::Unreachable) {
        end -= 1;
    }
    &body[..end]
}

/// Extract the local name from a block's result value.
/// Matches patterns like: `[..., LocalGet { name }, Br { depth: 0 }]`
/// or `[..., Seq([LocalGet { name }, Br { depth: 0 }])]`.
///
/// Only safe when the block has a single exit point (one `Br { depth: 0 }`):
/// with multiple exits, another path may hand out a different local.
///
/// Trailing `Unreachable` instructions are ignored — the code generator
/// may append `unreachable` after a `Br` so the Wasm validator accepts
/// the typed block even when the fallthrough path is dead.
fn extract_block_result_local(body: &[WirInstr]) -> Option<String> {
    if count_br_depth_zero(body) != 1 {
        return None;
    }
    let body = skip_trailing_unreachable(body);
    // Check last instruction(s) for LocalGet + Br pattern
    let len = body.len();
    if len >= 2
        && let WirInstr::Br { depth: 0 } = &body[len - 1]
        && let WirInstr::LocalGet { name, .. } = &body[len - 2]
    {
        return Some(name.clone());
    }
    // Check for Seq([LocalGet, Br]) as the last instruction
    if let Some(WirInstr::Seq(seq)) = body.last()
        && seq.len() >= 2
    {
        let slen = seq.len();
        if let WirInstr::Br { depth: 0 } = &seq[slen - 1]
            && let WirInstr::LocalGet { name, .. } = &seq[slen - 2]
        {
            return Some(name.clone());
        }
    }
    None
}

/// Extract a `StructNew` from a block's result value.
/// Matches patterns like: `[..., StructNew { ... }, Br { depth: 0 }]`
/// or `[..., Seq([StructNew { ... }, Br { depth: 0 }])]`.
/// Returns the (`type_id`, fields) cloned from the `StructNew`.
///
/// Only safe when the block has a single exit point (one `Br { depth: 0 }`).
/// Blocks with multiple exits (e.g., early `break` inside branches) may
/// produce different `StructNew` values on different paths.
///
/// Trailing `Unreachable` instructions are ignored — the code generator
/// may append `unreachable` after a `Br` so the Wasm validator accepts
/// the typed block even when the fallthrough path is dead.
fn extract_block_result_struct_new(body: &[WirInstr]) -> Option<(WirTypeId, Vec<WirInstr>)> {
    // Count Br { depth: 0 } in the block body. If there are multiple, the
    // block result is ambiguous and we cannot safely forward fields.
    if count_br_depth_zero(body) != 1 {
        return None;
    }

    let extract = |items: &[WirInstr]| -> Option<(WirTypeId, Vec<WirInstr>)> {
        let items = skip_trailing_unreachable(items);
        let len = items.len();
        if len >= 2
            && let WirInstr::Br { depth: 0 } = &items[len - 1]
            && let WirInstr::StructNew { type_id, fields } = &items[len - 2]
        {
            return Some((type_id.clone(), fields.clone()));
        }
        None
    };

    if let Some(result) = extract(body) {
        return Some(result);
    }
    let body = skip_trailing_unreachable(body);
    if let Some(WirInstr::Seq(seq)) = body.last() {
        return extract(seq);
    }
    None
}

/// Extract the local name from a Seq's result value: the tail instruction is
/// `LocalGet { name }`. Trailing `Unreachable` instructions are skipped.
fn extract_seq_result_local(body: &[WirInstr]) -> Option<String> {
    let body = skip_trailing_unreachable(body);
    if let Some(WirInstr::LocalGet { name, .. }) = body.last() {
        return Some(name.clone());
    }
    None
}

/// Extract a `StructNew` from a Seq's result value: the tail instruction is
/// `StructNew { ... }`. Trailing `Unreachable` instructions are skipped.
fn extract_seq_result_struct_new(body: &[WirInstr]) -> Option<(WirTypeId, Vec<WirInstr>)> {
    let body = skip_trailing_unreachable(body);
    if let Some(WirInstr::StructNew { type_id, fields }) = body.last() {
        return Some((type_id.clone(), fields.clone()));
    }
    None
}

/// Count the number of `Br { depth: 0 }` instructions in a block body,
/// adjusting depth when entering nested blocks/loops.
fn count_br_depth_zero(body: &[WirInstr]) -> usize {
    let mut count = 0;
    for instr in body {
        count_br_depth_zero_in_instr(instr, 0, &mut count);
    }
    count
}

fn count_br_depth_zero_in_instr(instr: &WirInstr, nesting: u32, count: &mut usize) {
    match instr {
        WirInstr::Br { depth } => {
            if *depth == nesting {
                *count += 1;
            }
        }
        WirInstr::Block { body, .. } => {
            for child in body {
                count_br_depth_zero_in_instr(child, nesting + 1, count);
            }
        }
        WirInstr::Loop { body, .. } => {
            // Loop `Br { depth: 0 }` targets the loop itself (continue),
            // not the outer block. Increment nesting so those don't count.
            for child in body {
                count_br_depth_zero_in_instr(child, nesting + 1, count);
            }
        }
        WirInstr::If {
            condition,
            then_body,
            else_body,
            ..
        } => {
            // Condition is evaluated outside the if label scope
            count_br_depth_zero_in_instr(condition, nesting, count);
            // If body creates a Wasm label: Br { depth: 0 } inside targets
            // the if, not the enclosing block. Increment nesting.
            for child in then_body {
                count_br_depth_zero_in_instr(child, nesting + 1, count);
            }
            if let Some(eb) = else_body {
                for child in eb {
                    count_br_depth_zero_in_instr(child, nesting + 1, count);
                }
            }
        }
        WirInstr::Seq(body) => {
            for child in body {
                count_br_depth_zero_in_instr(child, nesting, count);
            }
        }
        _ => {
            instr.for_each_child(&mut |child| {
                count_br_depth_zero_in_instr(child, nesting, count);
            });
        }
    }
}

/// Copy all known field values from one local to another.
fn copy_field_knowledge(known: &mut FieldKnowledge<'_>, from: &str, to: &str) {
    if known.aliased.contains(to) {
        return;
    }
    let entries: Vec<(String, WirInstr)> = known
        .fields
        .iter()
        .filter(|((name, _), _)| name == from)
        .map(|((_, field), val)| (field.clone(), val.clone()))
        .collect();
    for (field, val) in entries {
        known.fields.insert((to.to_string(), field), val);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wir::{WirField, WirFuncId, WirMeta, WirName, WirStructType, WirType};

    fn local_set(name: &str, value: WirInstr) -> WirInstr {
        WirInstr::LocalSet {
            name: name.to_string(),
            value: Box::new(value),
        }
    }

    fn local_get(name: &str) -> WirInstr {
        WirInstr::LocalGet {
            name: name.to_string(),
            result_ty: WirType::I32,
        }
    }

    fn test_type_id() -> WirTypeId {
        WirTypeId::new(0, "test//S".into())
    }

    fn test_types() -> Vec<WirTypeDef> {
        vec![WirTypeDef::Struct(WirStructType {
            name: WirName {
                fq: "test//S".to_string(),
            },
            fields: vec![WirField {
                name: "f".to_string(),
                ty: WirType::I32,
                mutable: true,
            }],
            meta: WirMeta::default(),
            generic_origin: None,
            newtype_origin: None,
            supertype: None,
        })]
    }

    fn struct_new(field_value: WirInstr) -> WirInstr {
        WirInstr::StructNew {
            type_id: test_type_id(),
            fields: vec![field_value],
        }
    }

    fn struct_get(local: &str) -> WirInstr {
        WirInstr::StructGet {
            type_id: test_type_id(),
            field_name: "f".to_string(),
            expr: Box::new(local_get(local)),
            result_ty: WirType::I32,
        }
    }

    fn run_forward(body: &mut [WirInstr], types: &[WirTypeDef]) {
        let copy_groups = collect_copy_groups(body);
        let mut aliased = collect_aliased_locals(body, &[], 0);
        widen_aliased_across_copy_groups(&mut aliased, &copy_groups);
        run_forward_with(body, types, &aliased, &copy_groups);
    }

    /// Like `run_forward` but with a caller-provided aliased set — for tests
    /// exercising the flow-sensitive invalidation, where the conservative
    /// unknown-callee aliasing would mask the path under test.
    fn run_forward_with(
        body: &mut [WirInstr],
        types: &[WirTypeDef],
        aliased: &IndexSet<String>,
        copy_groups: &IndexMap<String, u32>,
    ) {
        let single_assigned = single_assigned_locals(body);
        let mut known = FieldKnowledge::new(types, aliased, &single_assigned, copy_groups);
        forward_fields_in_body(body, &mut known);
    }

    fn set_value(stmt: &WirInstr) -> &WirInstr {
        let WirInstr::LocalSet { value, .. } = stmt else {
            panic!("expected LocalSet, got {stmt:?}");
        };
        value.as_ref()
    }

    // A constant-bound local reassigned by a `local.tee` *nested* in a later
    // subexpression must not fold to the stale constant. The per-instruction
    // knowledge update only sees the outer `LocalSet`, so it misses the tee;
    // restricting `local_const` to single-assignment locals keeps the fold
    // sound. (The optimizer's copy-merge produces exactly such nested tees.)
    #[test]
    fn nested_tee_reassignment_blocks_const_fold() {
        let types = test_types();

        let mut body = vec![
            // a = 5  → a constant binding
            local_set("a", WirInstr::I32Const(5)),
            // b = (a := 10) + 0  → the tee reassigns `a` inside the subexpression
            local_set(
                "b",
                WirInstr::I32Add(
                    Box::new(WirInstr::LocalTee {
                        name: "a".to_string(),
                        value: Box::new(WirInstr::I32Const(10)),
                    }),
                    Box::new(WirInstr::I32Const(0)),
                ),
            ),
            // out = a  → must stay a `LocalGet`, not fold to the stale 5
            local_set("out", local_get("a")),
        ];

        let single_assigned = single_assigned_locals(&body);
        assert!(
            !single_assigned.contains("a"),
            "`a` is assigned twice (LocalSet + nested LocalTee), so it is not \
             single-assignment and must be ineligible for the const fold"
        );

        run_forward(&mut body, &types);

        assert!(
            matches!(set_value(&body[2]), WirInstr::LocalGet { name, .. } if name == "a"),
            "reassigned `a` must not fold to the stale constant, got {:?}",
            set_value(&body[2])
        );
    }

    // A `LocalSet` after a conditional block exit only executes on the
    // fall-through path; its field facts must not survive the block.
    #[test]
    fn block_exit_confines_later_facts() {
        let types = test_types();

        let mut body = vec![
            local_set("a", struct_new(WirInstr::I32Const(3))),
            WirInstr::Block {
                label: Some("outer".to_string()),
                result: None,
                body: vec![
                    WirInstr::BrIf {
                        depth: 0,
                        condition: Box::new(local_get("c")),
                    },
                    local_set("a", struct_new(WirInstr::I32Const(4))),
                ],
            },
            local_set("out", struct_get("a")),
        ];

        run_forward(&mut body, &types);

        assert!(
            matches!(set_value(&body[2]), WirInstr::StructGet { .. }),
            "a.f after the block is 3 or 4 depending on the exit; it must \
             not fold, got {:?}",
            set_value(&body[2])
        );
    }

    // Facts recorded before the first possible exit hold on every path and
    // must flow through the block (precision guard for the exit handling).
    #[test]
    fn block_facts_before_exit_flow_through() {
        let types = test_types();

        let mut body = vec![
            WirInstr::Block {
                label: Some("outer".to_string()),
                result: None,
                body: vec![
                    local_set("a", struct_new(WirInstr::I32Const(3))),
                    WirInstr::BrIf {
                        depth: 0,
                        condition: Box::new(local_get("c")),
                    },
                ],
            },
            local_set("out", struct_get("a")),
        ];

        run_forward(&mut body, &types);

        assert!(
            matches!(set_value(&body[1]), WirInstr::I32Const(3)),
            "a.f is 3 on both block exits and must fold, got {:?}",
            set_value(&body[1])
        );
    }

    // A struct-typed local reassigned by a def nested in a non-body position
    // (here a `Drop(LocalTee(...))` statement) must lose its field facts.
    #[test]
    fn nested_tee_reassignment_invalidates_fields() {
        let types = test_types();

        let mut body = vec![
            local_set("a", struct_new(WirInstr::I32Const(1))),
            WirInstr::Drop(Box::new(WirInstr::LocalTee {
                name: "a".to_string(),
                value: Box::new(local_get("z")),
            })),
            local_set("out", struct_get("a")),
        ];

        run_forward(&mut body, &types);

        assert!(
            matches!(set_value(&body[2]), WirInstr::StructGet { .. }),
            "a.f must not fold to the stale 1 after the nested tee, got {:?}",
            set_value(&body[2])
        );
    }

    // A call inside an `if` arm may mutate a local passed to it; the merge
    // must invalidate that local's facts. Runs with an empty aliased set,
    // modelling a callee without `stores` — the merge invalidation alone
    // must protect the read.
    #[test]
    fn call_in_if_arm_invalidates() {
        let types = test_types();

        let mut body = vec![
            local_set("a", struct_new(WirInstr::I32Const(1))),
            WirInstr::If {
                condition: Box::new(local_get("c")),
                result: None,
                then_body: vec![WirInstr::Call {
                    func_id: WirFuncId::new(0, "test//mutate".into()),
                    args: vec![local_get("a")],
                }],
                else_body: None,
            },
            local_set("out", struct_get("a")),
        ];

        run_forward_with(
            &mut body,
            &types,
            &IndexSet::default(),
            &IndexMap::default(),
        );

        assert!(
            matches!(set_value(&body[2]), WirInstr::StructGet { .. }),
            "a.f may be mutated by the conditional call and must not fold, got {:?}",
            set_value(&body[2])
        );
    }

    // A mutating call nested inside a statement's value (not a top-level
    // call statement) must invalidate the locals it receives.
    #[test]
    fn call_nested_in_local_set_invalidates() {
        let types = test_types();

        let mut body = vec![
            local_set("a", struct_new(WirInstr::I32Const(1))),
            local_set(
                "s",
                WirInstr::Call {
                    func_id: WirFuncId::new(0, "test//mutate".into()),
                    args: vec![local_get("a")],
                },
            ),
            local_set("out", struct_get("a")),
        ];

        run_forward_with(
            &mut body,
            &types,
            &IndexSet::default(),
            &IndexMap::default(),
        );

        assert!(
            matches!(set_value(&body[2]), WirInstr::StructGet { .. }),
            "a.f may be mutated by the nested call and must not fold, got {:?}",
            set_value(&body[2])
        );
    }

    // A plain local-to-local copy forwards field knowledge, and a mutation
    // through one copy invalidates the whole group.
    #[test]
    fn copy_forwards_fields_and_mutation_invalidates_group() {
        let types = test_types();

        let mut body = vec![
            local_set("a", struct_new(WirInstr::I32Const(1))),
            local_set("b", local_get("a")),
            // out1 = b.f → folds to 1 through the copy
            local_set("out1", struct_get("b")),
            // b.f = 9 → mutates the shared object; invalidates a's fact too
            WirInstr::StructSet {
                type_id: test_type_id(),
                field_name: "f".to_string(),
                expr: Box::new(local_get("b")),
                value: Box::new(WirInstr::I32Const(9)),
            },
            local_set("out2", struct_get("a")),
        ];

        run_forward(&mut body, &types);

        assert!(
            matches!(set_value(&body[2]), WirInstr::I32Const(1)),
            "b.f before the mutation must fold through the plain copy, got {:?}",
            set_value(&body[2])
        );
        assert!(
            matches!(set_value(&body[4]), WirInstr::StructGet { .. }),
            "a.f after the mutation through the copy `b` must not fold, got {:?}",
            set_value(&body[4])
        );
    }
}
