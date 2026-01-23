# Plan: Second-Level LICM via Immutable Reference Look-Through

## Problem

After inlining, code contains patterns like:

```wado
let _licm_entries = self.entries;  // hoisted by LICM
for ... {
    __inline_block: {
        let self: &Array<T> = &_licm_entries;  // immutable ref to hoisted value
        ... self.repr ...  // NOT hoisted - self is defined in loop
    };
}
```

The `self.repr` access is loop-invariant because:

1. `self` is an immutable reference (`&T`), so `self.repr` cannot be modified through it
2. `_licm_entries` is loop-invariant (already hoisted)
3. Therefore `_licm_entries.repr` is also loop-invariant

Current LICM doesn't hoist `self.repr` because `self` is _defined_ inside the loop (added to `modified_vars`). But since `&T` guarantees immutability, we can safely look through the reference.

## Proposed Solution: Look Through Immutable References in LICM

Enhance LICM's candidate finding to look through immutable reference bindings.

### Key Insight

In Wado, `&T` (immutable reference) guarantees the referenced value cannot be modified. So:

- `let self: &T = &x;` followed by `self.field`
- Is semantically equivalent to `x.field`
- If `x` is loop-invariant, so is `self.field`

### Implementation

**File:** `wado-compiler/src/optimize.rs`

#### Step 1: Collect Immutable Reference Bindings

Before finding hoist candidates, scan the loop body for immutable reference patterns:

```rust
/// Map from reference local index to source local index
/// For patterns like: `let ref_var: &T = &source_var`
fn collect_immutable_ref_bindings(block: &TirBlock, type_table: &TypeTable) -> HashMap<u32, u32> {
    let mut bindings = HashMap::new();
    collect_ref_bindings_in_block(block, type_table, &mut bindings);
    bindings
}

fn collect_ref_bindings_in_block(block: &TirBlock, type_table: &TypeTable, bindings: &mut HashMap<u32, u32>) {
    for stmt in &block.stmts {
        match &stmt.kind {
            TirStmtKind::Let { local_index, value, type_id, .. } => {
                // Check if this is: let x: &T = &y
                if is_immutable_ref_type(*type_id, type_table) {
                    if let TirExprKind::Unary { op: TirUnaryOp::Ref, expr } = &value.kind {
                        if let TirExprKind::Local { index: source_idx, .. } = &expr.kind {
                            bindings.insert(*local_index, *source_idx);
                        }
                    }
                }
            }
            // Recurse into nested blocks, labeled blocks, etc.
            TirStmtKind::LabeledBlock { block, .. } => {
                collect_ref_bindings_in_block(block, type_table, bindings);
            }
            // ... other cases
        }
    }
}

fn is_immutable_ref_type(type_id: TypeId, type_table: &TypeTable) -> bool {
    matches!(type_table.get(type_id), Some(Type::Ref { mutable: false, .. }))
}
```

#### Step 2: Enhance Hoist Candidate Finding

Modify `find_hoist_candidates_in_expr` to look through immutable references:

```rust
fn find_hoist_candidates_in_expr(
    expr: &TirExpr,
    modified_vars: &HashSet<u32>,
    ref_bindings: &HashMap<u32, u32>,  // NEW parameter
    candidates: &mut Vec<HoistCandidate>,
    seen: &mut HashSet<(u32, u32)>,
    next_local: &mut u32,
) {
    match &expr.kind {
        TirExprKind::FieldAccess { expr: inner, field_index, field_name, .. } => {
            if let TirExprKind::Local { index, name, .. } = &inner.kind {
                // Try direct hoist first
                if !modified_vars.contains(index) {
                    // existing hoist logic
                }
                // NEW: Look through immutable reference
                else if let Some(source_index) = ref_bindings.get(index) {
                    if !modified_vars.contains(source_index) {
                        // Can hoist! Use source variable instead
                        let key = (*source_index, *field_index);
                        if !seen.contains(&key) {
                            seen.insert(key);
                            candidates.push(HoistCandidate {
                                local_index: *source_index,
                                local_name: /* need to get source name */,
                                field_index: *field_index,
                                field_name: field_name.clone(),
                                type_id: expr.type_id,
                                new_local_index: *next_local,
                            });
                            *next_local += 1;
                        }
                    }
                }
            }
            // Still recurse
            find_hoist_candidates_in_expr(inner, modified_vars, ref_bindings, candidates, seen, next_local);
        }
        // ... update other recursive calls to pass ref_bindings
    }
}
```

#### Step 3: Update Replacement Logic

When replacing `self.field` where `self` is a reference to `source`, we need to:

1. Replace `self.field` with `_licm_var` (the hoisted local)
2. The hoisting statement reads from `source.field`, not `self.field`

```rust
fn replace_hoisted_in_expr(expr: &mut TirExpr, candidates: &[HoistCandidate], ref_bindings: &HashMap<u32, u32>) {
    match &mut expr.kind {
        TirExprKind::FieldAccess { expr: inner, field_index, .. } => {
            if let TirExprKind::Local { index, .. } = &inner.kind {
                // Check direct match
                if let Some(candidate) = candidates.iter().find(|c|
                    c.local_index == *index && c.field_index == *field_index
                ) {
                    // Replace with hoisted local
                    *expr = TirExpr::new(TirExprKind::Local { ... }, ...);
                    return;
                }
                // NEW: Check if this is a reference to a hoisted source
                if let Some(source_index) = ref_bindings.get(index) {
                    if let Some(candidate) = candidates.iter().find(|c|
                        c.local_index == *source_index && c.field_index == *field_index
                    ) {
                        // Replace with hoisted local
                        *expr = TirExpr::new(TirExprKind::Local { ... }, ...);
                        return;
                    }
                }
            }
            replace_hoisted_in_expr(inner, candidates, ref_bindings);
        }
        // ... other cases
    }
}
```

#### Step 4: Update Call Sites

Update `licm_loop` to collect and pass `ref_bindings`:

```rust
fn licm_loop(
    loop_body: &mut TirBlock,
    local_count: &mut u32,
    local_types: &mut Vec<TypeId>,
    type_table: &TypeTable,
    extra_modified: &HashSet<u32>,
) -> Vec<TirStmt> {
    let mut modified_vars = extra_modified.clone();
    collect_modified_vars_in_block(loop_body, &mut modified_vars);

    // NEW: Collect immutable reference bindings
    let ref_bindings = collect_immutable_ref_bindings(loop_body, type_table);

    let mut candidates = Vec::new();
    let mut seen = HashSet::new();
    let mut next_local = *local_count;
    find_hoist_candidates_in_block(
        loop_body,
        &modified_vars,
        &ref_bindings,  // NEW
        &mut candidates,
        &mut seen,
        &mut next_local,
    );

    // ... rest of function, also pass ref_bindings to replace functions
}
```

### Expected Result

```wado
// Before (current)
fn "MiniDict<String,String>::has"(...) -> bool {
    let _licm_entries: Array<[String, String]> = self.entries;
    for let mut i: i32 = 0; ... {
        if "String^Eq::eq"(&__inline_..._0: {
            let self: &Array<[String, String]> = &_licm_entries;
            break ...: core::builtin::array_get::<[String, String]>(self.repr, i);
        }.0, &key) {
            return true;
        }
    }
    return false;
}

// After (with immutable ref look-through)
fn "MiniDict<String,String>::has"(...) -> bool {
    let _licm_entries: Array<[String, String]> = self.entries;
    let _licm_repr: builtin::array<[String, String]> = _licm_entries.repr;  // NEW
    for let mut i: i32 = 0; ... {
        if "String^Eq::eq"(&__inline_..._0: {
            break ...: core::builtin::array_get::<[String, String]>(_licm_repr, i);  // Uses hoisted
        }.0, &key) {
            return true;
        }
    }
    return false;
}
```

### Scope

1. Only handle immutable references (`&T`, not `&mut T`)
2. Only handle simple patterns: `let x: &T = &local_var`
3. Skip complex cases like `let x: &T = &expr.field` (could extend later)

### Testing

1. All existing e2e tests must pass
2. Check `dict_mini.lowered.wado` - should show `_licm_repr` hoisted
3. Check `array_index_inline_nested.lowered.wado` - should show second-level hoisting
4. Run `make benchmark-sieve` - verify no regression, possibly improvement

### Risk

Low:

- Semantically safe: `&T` guarantees no mutation through the reference
- Conservative: only handles simple, clear patterns
- Additive: enhances existing LICM, doesn't change its core logic

## Implementation Checklist

- [x] Add `is_immutable_ref_type` helper function
- [x] Add `collect_immutable_ref_bindings` function (as `collect_licm_ref_bindings_*`)
- [x] Update `find_hoist_candidates_in_block/expr` to accept and use `ref_bindings`
- [x] Update `replace_hoisted_in_block/expr` to handle reference look-through
- [x] Update `licm_loop` to collect and pass `ref_bindings`
- [x] Make LICM iterative with max 10 iterations for second-level hoisting
- [x] Fix: Recurse into Break statement values for all LICM functions
- [x] Run tests: `cargo test -p wado-compiler`
- [x] Update golden fixtures: `make update-golden-fixtures`
- [x] Run benchmarks: `make benchmark-sieve` (75-79ms, down from original 95ms)
- [x] Run full validation: `make on-task-done`
