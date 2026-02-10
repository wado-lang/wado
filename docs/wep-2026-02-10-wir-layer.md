# WEP: WIR (Wasm IR) Layer

## Context

The Wado compiler pipeline currently flows:

```
Source → Lexer → Parser → Bind → Load → Analyze → Resolve → Effect Check
→ Monomorphize → Lower → Optimize → Wasm Plan → Codegen → Wasm bytes
```

`codegen.rs` is the largest file in the compiler (11,775 lines, 97 panics) and conflates three distinct concerns:

1. **Planning**: Deciding Wasm GC type layouts, assigning func/type/local indices, resolving call targets
2. **Lowering**: Translating TIR semantics to Wasm GC operations (value copy, Option as nullable ref, variant as tagged struct)
3. **Emission**: Encoding `wasm_encoder::Instruction` into bytes

Additionally, the Lower phase (8,061 lines) produces a "post-lower TIR" that is structurally very different from "pre-lower TIR":

- Dead nodes exist that panic in codegen (`IfPattern`, `Closure`, `Capture`, i128 patterns)
- The type table is mutated (box lowering rewrites `Ref(Primitive)` to `Struct(__Box_T)`)
- Synthetic types and functions are injected (closure structs, `__call` methods, `__initialize_module`)
- Scratch local analysis is performed for codegen's benefit

The optimizer (10,547 lines across 7 modules) runs on this post-lower TIR, but several passes exist solely to bridge TIR and codegen:

- `optimize_rewrite.rs` inserts `Move` nodes (a TIR-to-codegen communication channel for skipping value copies)
- `optimize_rewrite.rs` collects `needed_copy_types` (codegen preparation)
- `optimize_dce.rs` has 500+ lines of name resolution logic that wouldn't be needed with resolved indices

### Problems

- **No place for Wasm-level optimization**: Peephole patterns like `StructGet(StructNew(fields), i) → fields[i]` cannot be expressed at TIR level (no Wasm indices) or codegen level (flat instruction stream)
- **Dead TIR nodes**: Several `TirExprKind` variants exist only as pre-lower forms and panic in codegen
- **codegen does analysis**: Type registration (~1,900 lines), `type_id_to_valtype` conversion (~260 lines), value copy generation (~550 lines), call resolution (~1,100 lines) are all analysis/decision-making, not emission
- **`wasm_plan` is undersized**: At 788 lines, it only handles CM boundary analysis and type ordering — insufficient to separate planning from emission
- **Optimizer bridges TIR and codegen**: `Move` nodes, `needed_copy_types`, `copy_source_types`, scratch local analysis are all codegen preparation that pollutes TIR

## Decision

Introduce **WIR (Wasm IR)**, a tree-structured IR with resolved Wasm indices, as the boundary between language semantics (TIR) and Wasm operations. Lower produces WIR directly from TIR. The optimizer moves from TIR to WIR.

### Pipeline

```
Source → ... → Resolve → Effect Check → Monomorphize
→ Lower (TIR→WIR) → Optimize (WIR→WIR) → Expand → Emit → Wasm bytes
```

| Phase | Input | Output | Description |
|-------|-------|--------|-------------|
| Monomorphize | TIR | TIR | Instantiate generics with concrete types |
| Lower | TIR | WIR | All planning + lowering decisions in one pass |
| Optimize | WIR | WIR | Inline, DCE, GC alloc elim, copy prop, const fold, LICM |
| Expand | WIR | WIR | Expand `ValueCopy` nodes to Wasm GC operation sequences |
| Emit | WIR | bytes | Mechanical tree flattening to `wasm_encoder` |

The boundary is clear: everything before Lower works with TIR (language semantics). Everything after works with WIR (Wasm operations).

### Design Principles

#### WIR nodes map 1:1 to Wasm GC instructions, in tree form

Each `WirExpr` node corresponds to one Wasm instruction. The tree structure captures operand relationships that are implicit in the stack machine. The single exception is `ValueCopy`, an explicit expansion point.

This means:

- Emit is purely mechanical (tree flatten → instruction stream, zero decisions)
- Peephole optimization is natural pattern matching on the tree
- WIR dump shows exactly what WAT will be generated

#### All indices are resolved

Type indices, function indices, local indices, global indices, data segment offsets — all resolved during Lower. No lookups in Optimize or Emit.

#### Statement/Expression split mirrors Wasm semantics

- `WirExpr`: produces exactly one value on the Wasm stack
- `WirStmt`: side effects only, no value

### WIR Data Structures

#### Top-Level

```rust
/// Complete WIR representation of a Wasm Component
struct WirComponent {
    /// Main core module (Wado-compiled code)
    main_module: WirModule,
    /// Memory module (CM boundary shared memory + realloc)
    memory_module: WirMemoryModule,
    /// Pre-compiled bundled modules (fts, libm — opaque blobs)
    bundled_modules: Vec<BundledModule>,
    /// Component-level structure (declarative)
    component: WirComponentStructure,
}

/// Core Wasm module
struct WirModule {
    types: Vec<WirTypeDef>,
    imports: Vec<WirImport>,
    functions: Vec<WirFunction>,
    tables: Vec<WirTable>,
    globals: Vec<WirGlobal>,
    data: Vec<u8>,              // concatenated string literal bytes
    elements: Vec<WirElement>,
    exports: Vec<WirExport>,
    names: WirNames,
}
```

#### Type Definitions

```rust
enum WirTypeDef {
    Struct { name: Option<String>, fields: Vec<WirField> },
    Array { name: Option<String>, element: WirField },
    Func { params: Vec<ValType>, results: Vec<ValType> },
    Rec(Vec<WirTypeDef>), // mutual recursion group
}
```

#### Functions

```rust
struct WirFunction {
    type_idx: u32,
    locals: Vec<WirLocal>,
    body: WirBody,
    name: Option<String>,
    branch_hints: Vec<WirBranchHint>,
    metadata: WirFunctionMetadata,
}

struct WirFunctionMetadata {
    is_pure: bool,
    is_recursive: bool,
    expr_count: usize,
    returns_never: bool,
}

struct WirBody {
    stmts: Vec<WirStmt>,
}
```

#### Statement Nodes

```rust
enum WirStmt {
    // Stores
    LocalSet { idx: u32, value: WirExpr },
    GlobalSet { idx: u32, value: WirExpr },
    StructSet { type_idx: u32, field_idx: u32, obj: WirExpr, value: WirExpr },
    ArraySet { type_idx: u32, arr: WirExpr, index: WirExpr, value: WirExpr },
    ArrayCopy {
        dst_type: u32, dst: WirExpr, dst_offset: WirExpr,
        src_type: u32, src: WirExpr, src_offset: WirExpr,
        len: WirExpr,
    },
    ArrayFill { type_idx: u32, arr: WirExpr, offset: WirExpr, value: WirExpr, len: WirExpr },
    Store { op: Instruction, addr: WirExpr, value: WirExpr },

    // Control flow
    Block { body: WirBody },
    Loop { body: WirBody },
    If { cond: WirExpr, then_body: WirBody, else_body: Option<WirBody>, hint: Option<BranchHint> },
    Br { depth: u32 },
    BrIf { depth: u32, cond: WirExpr, hint: Option<BranchHint> },
    BrTable { targets: Vec<u32>, default: u32, index: WirExpr },
    Return { value: Option<WirExpr> },
    Unreachable,

    // Side-effect expressions
    Drop(WirExpr),
    Exec(WirExpr), // void call etc.
}
```

#### Expression Nodes

```rust
enum WirExpr {
    // Constants
    I32Const(i32),
    I64Const(i64),
    F32Const(f32),
    F64Const(f64),

    // Locals/Globals
    LocalGet { idx: u32 },
    LocalTee { idx: u32, value: Box<WirExpr> },
    GlobalGet { idx: u32 },

    // Arithmetic, comparison, logic (op is wasm_encoder::Instruction)
    BinOp { op: Instruction, lhs: Box<WirExpr>, rhs: Box<WirExpr> },
    UnOp { op: Instruction, operand: Box<WirExpr> },

    // Type conversion
    Convert { op: Instruction, operand: Box<WirExpr> },

    // GC Struct
    StructNew { type_idx: u32, fields: Vec<WirExpr> },
    StructGet { type_idx: u32, field_idx: u32, obj: Box<WirExpr> },

    // GC Array
    ArrayNew { type_idx: u32, size: Box<WirExpr>, init: Box<WirExpr> },
    ArrayNewDefault { type_idx: u32, size: Box<WirExpr> },
    ArrayNewData { type_idx: u32, data_idx: u32, offset: Box<WirExpr>, size: Box<WirExpr> },
    ArrayNewFixed { type_idx: u32, elements: Vec<WirExpr> },
    ArrayGet { type_idx: u32, arr: Box<WirExpr>, index: Box<WirExpr> },
    ArrayGetS { type_idx: u32, arr: Box<WirExpr>, index: Box<WirExpr> },
    ArrayGetU { type_idx: u32, arr: Box<WirExpr>, index: Box<WirExpr> },
    ArrayLen { arr: Box<WirExpr> },

    // References
    RefNull { heap_type: HeapType },
    RefIsNull { expr: Box<WirExpr> },
    RefAsNonNull { expr: Box<WirExpr> },
    RefCast { type_idx: u32, nullable: bool, expr: Box<WirExpr> },
    RefTest { type_idx: u32, nullable: bool, expr: Box<WirExpr> },
    RefEq { lhs: Box<WirExpr>, rhs: Box<WirExpr> },
    RefFunc { func_idx: u32 },

    // Calls
    Call { func_idx: u32, args: Vec<WirExpr> },
    CallRef { type_idx: u32, args: Vec<WirExpr>, func_ref: Box<WirExpr> },

    // Memory loads
    Load { op: Instruction, addr: Box<WirExpr> },

    // Control flow (value-producing)
    If { result_type: BlockType, cond: Box<WirExpr>, then_expr: Box<WirExpr>, else_expr: Box<WirExpr> },
    Block { result_type: BlockType, body: Vec<WirStmt>, result: Box<WirExpr> },

    // Select
    Select { val_type: ValType, cond: Box<WirExpr>, if_true: Box<WirExpr>, if_false: Box<WirExpr> },

    // Value copy (expansion point — expanded by Expand phase before Emit)
    ValueCopy { kind: CopyKind, source: Box<WirExpr> },
}
```

#### ValueCopy

`ValueCopy` is the single exception to the "1:1 with Wasm instructions" principle. It is kept as a high-level node for:

1. **Debuggability**: WIR dump shows "this is a copy" rather than a sea of `StructGet`/`StructNew`
2. **Copy elimination**: The optimizer can match `ValueCopy { source: StructNew { .. } }` and eliminate the copy
3. **Deferred expansion**: The Expand phase converts it to Wasm GC operation sequences before Emit

```rust
enum CopyKind {
    Struct { type_idx: u32, field_count: u32 },
    Array { array_struct_type_idx: u32, raw_array_type_idx: u32 },
    Variant { base_type_idx: u32, cases: Vec<VariantCaseCopy> },
    Option { inner: Box<CopyKind> },
}
```

#### Wasm Instruction Reuse

`BinOp`, `UnOp`, `Convert`, `Store`, and `Load` use `wasm_encoder::Instruction` directly for the operation kind. This eliminates ~170 lines of enum re-definition and conversion code. The type safety loss is acceptable because only Lower creates these nodes.

#### Component-Level Structure

CM boundary is split into two parts:

- **Declarative**: `WirComponentStructure` holds canonical lower/lift declarations, WASI imports, exports
- **Imperative**: Adapter functions (HTTP handler return, CM effect wrappers) are regular `WirFunction` bodies in `WirModule`

```rust
struct WirComponentStructure {
    wasi_imports: Vec<WirWasiImport>,
    canonical_lowers: Vec<WirCanonicalLower>,
    canonical_lifts: Vec<WirCanonicalLift>,
    exports: Vec<WirComponentExport>,
    instances: Vec<WirInstance>,
}
```

Adapter functions as WIR bodies enables future auto-generation of CM adapters and allows WIR optimization to apply to glue code.

### Lower (TIR → WIR)

Lower is the single TIR→WIR conversion point. It replaces the current `lower.rs`, `wasm_plan.rs`, and most of `codegen.rs`.

#### Internal Structure

```
Lower (TIR → WIR)
├── Plan sub-phase
│   ├── Collect all types → plan GC type defs (struct, array, variant, closure struct)
│   ├── Topologically sort types, assign type indices
│   ├── Collect all functions (user + synthetic __call, __initialize_module)
│   ├── Assign function indices
│   ├── Plan imports (WASI, builtins) and exports
│   ├── Collect string literals → build data segment with offsets
│   └── CM boundary analysis (ComponentPlan)
│
└── Build sub-phase (per function)
    ├── Allocate local variables
    └── Lower TIR body → WIR body
        ├── IfPattern → If + RefTest / RefIsNull
        ├── Closure → StructNew (closure struct)
        ├── Capture → StructGet (closure field)
        ├── &primitive → StructNew (box struct)
        ├── *box_ref → StructGet (box value field)
        ├── Match / Switch → Block + BrTable
        ├── Binary(Add, type:i32) → BinOp(I32Add)
        ├── FieldAccess → StructGet
        ├── StringLiteral → StructNew(String, [ArrayNewData(...)])
        ├── VariantConstruct → StructNew (case struct)
        ├── OptionSome → value expr (for ref types)
        ├── Null → RefNull
        └── ValueCopy insertion where needed
```

#### What Lower Absorbs

| Current module | Lines | Absorbed into |
|---------------|-------|---------------|
| `lower.rs` | 8,061 | Lower (Build sub-phase) |
| `wasm_plan.rs` | 788 | Lower (Plan sub-phase) |
| `codegen.rs` type registration | ~1,900 | Lower (Plan sub-phase) |
| `codegen.rs` type_id_to_valtype | ~260 | Lower (Plan sub-phase, cached in valtype map) |
| `codegen.rs` value copy generation | ~550 | Lower (ValueCopy insertion) + Expand |
| `codegen.rs` call resolution | ~1,100 | Lower (Plan sub-phase, func index table) |
| `codegen.rs` generate_expr | ~2,200 | Lower (Build sub-phase) |
| `codegen.rs` generate_stmt | ~430 | Lower (Build sub-phase) |
| `codegen.rs` CM boundary | ~620 | Lower (Plan + adapter function generation) |
| `component_model.rs` | 1,720 | Lower (Plan sub-phase) |
| `copy_context.rs` | 226 | Expand phase |
| `wasm_builder.rs` | 649 | Lower (Plan sub-phase) |
| `wasm_postprocess.rs` | — | Eliminated (DCE at WIR level) |

### Optimizer on WIR

The optimizer moves from TIR to WIR. Several passes become simpler; others are eliminated.

#### Pass Migration

| Pass | TIR (current) | WIR (proposed) | Change |
|------|---------------|----------------|--------|
| inline | 3,265 lines | ~2,000 lines | Simpler: `func_idx` direct lookup, no name resolution |
| ref_elim | 1,019 lines | ~200 lines | Subsumed by GC allocation elimination |
| copy_prop | 1,357 lines | ~800 lines | Standard `LocalSet`/`LocalGet` propagation |
| const_fold | 549 lines | ~300 lines | Simpler: Wasm opcode encodes type, no newtype traversal |
| licm | 1,605 lines | ~1,200 lines | Similar: `WirStmt::Loop` + `WirExpr::StructGet` hoisting |
| rewrite | 703 lines | ~50 lines | Select lowering only. Move and value-copy-types eliminated |
| dce | 2,049 lines | ~500 lines | Trivial: `func_idx` reachability, no name resolution |
| **Total** | **10,547** | **~5,050** | **~52% reduction** |

#### New Optimizations Enabled by WIR

These optimizations are natural on WIR's tree structure but impossible at TIR level:

##### GC Allocation Elimination

```
StructGet { type_idx: T, field_idx: i, obj: StructNew { type_idx: T, fields } }
→ fields[i]
```

This single rule subsumes:

- Reference elimination (box created then immediately unboxed)
- Value copy elimination (`ValueCopy` of a freshly constructed value)
- Temporary struct elimination (tuple created for multi-return then destructured)

##### Null Check Elimination

```
RefIsNull { expr: StructNew { .. } }  →  I32Const(0)
```

##### Cast Elimination

```
RefCast { type_idx: T, expr: StructNew { type_idx: T, .. } }
→ StructNew { type_idx: T, .. }
```

##### Constant Folding (Wasm-native)

```
BinOp { op: I32Add, lhs: I32Const(a), rhs: I32Const(b) }  →  I32Const(a + b)
BinOp { op: I32Add, lhs: expr, rhs: I32Const(0) }  →  expr
BinOp { op: I32Mul, lhs: expr, rhs: I32Const(1) }  →  expr
```

##### ValueCopy Elimination

```
ValueCopy { source: StructNew { .. } }  →  StructNew { .. }
ValueCopy { source: Call { .. } }  →  Call { .. }
```

Any freshly constructed value does not need copying.

#### Inline Eligibility on WIR

Lower computes `WirFunctionMetadata` from TIR semantic information and attaches it to each `WirFunction`. The WIR inliner reads metadata instead of analyzing TIR:

```rust
struct WirFunctionMetadata {
    is_pure: bool,        // no effects
    is_recursive: bool,   // self-referencing call
    expr_count: usize,    // for threshold check
    returns_never: bool,  // divergent function
}
```

### TIR Move Node Elimination

The `TirExprKind::Move` node — inserted by `optimize_rewrite.rs` to tell codegen "skip value copy for this expression" — is eliminated. `ValueCopy` in WIR inverts the communication:

| Current (Move) | WIR (ValueCopy) |
|----------------|-----------------|
| Mark what does NOT need copy | Mark what DOES need copy |
| Optimizer inserts `Move` | Lower inserts `ValueCopy` |
| Codegen checks `Move` absence | Optimizer removes unnecessary `ValueCopy` |

Lower determines copy necessity via `is_fresh()`:

```rust
fn is_fresh(expr: &TirExpr) -> bool {
    matches!(&expr.kind,
        TirExprKind::StructLiteral { .. }
        | TirExprKind::ArrayLiteral { .. }
        | TirExprKind::TupleLiteral { .. }
        | TirExprKind::Call { .. }
        | TirExprKind::StaticCall { .. }
        | TirExprKind::MethodCall { .. }
        | TirExprKind::VariantConstruct { .. }
        | TirExprKind::OptionSome { .. }
    )
}
```

WIR Optimize provides a safety net: even if Lower is conservative and inserts too many `ValueCopy` nodes, GC allocation elimination and `ValueCopy` elimination remove them.

Cascading removals from TIR:

- `TirExprKind::Move`
- `optimize_rewrite.rs` Move insertion logic
- `TirFunction::needed_copy_types`
- `TirFunction::copy_source_types`
- `optimize.rs` `expand_copy_source_types()`
- `copy_context.rs` (absorbed into Expand phase)

### Branch Hints

Branch hints are attached at Lower time (WIR Build sub-phase) as optional metadata on `WirStmt::If` and `WirStmt::BrIf`. The semantic context for hint decisions (e.g., "Option::None check is unlikely") is available during TIR→WIR conversion and lost afterward.

```rust
enum BranchHint { Likely, Unlikely }

enum WirStmt {
    If { cond: WirExpr, then_body: WirBody, else_body: Option<WirBody>, hint: Option<BranchHint> },
    BrIf { depth: u32, cond: WirExpr, hint: Option<BranchHint> },
    // ...
}
```

### Dump Support

The existing `wado dump` phases extend naturally:

```sh
wado dump --tir file.wado          # TIR (before monomorphization)
wado dump --tir --unparse file.wado   # TIR as pseudo-Wado source
wado dump --wir file.wado          # WIR after Lower (replaces --lower)
wado dump --wir --unparse file.wado   # WIR as pseudo-WAT
wado dump --optimize file.wado     # WIR after Optimize
```

### File Structure

```
wir.rs              ~500 lines   WIR data structures
lower.rs            ~8,500 lines TIR→WIR (Plan + Build, replaces current lower.rs + codegen analysis)
wir_optimize.rs     ~5,050 lines WIR optimization passes
wir_expand.rs       ~300 lines   ValueCopy expansion
wir_emit.rs         ~800 lines   Mechanical Wasm emission
```

### Superseded Modules

| Module | Disposition |
|--------|------------|
| `lower.rs` (current) | Replaced by new `lower.rs` (TIR→WIR) |
| `codegen.rs` | Replaced by `lower.rs` + `wir_emit.rs` |
| `wasm_plan.rs` | Absorbed into `lower.rs` Plan sub-phase |
| `component_model.rs` | Absorbed into `lower.rs` Plan sub-phase |
| `wasm_builder.rs` | Absorbed into `lower.rs` Plan sub-phase |
| `copy_context.rs` | Absorbed into `wir_expand.rs` |
| `wasm_postprocess.rs` | Eliminated (bundled DCE at WIR level) |
| `optimize_rewrite.rs` | Replaced by select lowering in `wir_optimize.rs` (~50 lines) |
| `optimize_ref_elim.rs` | Subsumed by GC allocation elimination in `wir_optimize.rs` |

## Consequences

### Benefits

- **~55% code reduction**: Current lower+optimize+codegen totals ~33,800 lines. Proposed WIR pipeline totals ~15,150 lines
- **Wasm-level optimization**: Peephole patterns (GC alloc elimination, constant folding, cast elimination) are expressible for the first time
- **Dead TIR nodes eliminated at the type level**: `IfPattern`, `Closure`, `Capture`, i128 patterns do not exist in `WirExpr`
- **Clean architectural boundary**: TIR = language semantics, WIR = Wasm operations. No ambiguous "post-lower TIR" state
- **Mechanical Emit**: ~800 lines with zero decisions, zero lookups, zero panics
- **Optimizer simplification**: Name resolution eliminated, `ref_elim` subsumed, `Move`/value-copy-types machinery removed
- **CM adapter generation path**: Adapter functions as WIR bodies enables future auto-generation

### Trade-offs

- **Large refactoring scope**: Lower, all optimizer passes, and codegen are rewritten
- **Loss of TIR-level dump for post-lower**: `--lower --unparse` currently shows pseudo-Wado; with WIR it shows pseudo-WAT. This is arguably more useful for Wasm debugging but less intuitive for language debugging
- **Optimizer loses named identifiers**: WIR uses indices instead of names. Debugging optimizer passes requires reverse lookups. Mitigated by optional `name` fields on `WirFunction` and `WirLocal`
- **Lower becomes the largest single pass**: ~8,500 lines. However, it has clear internal structure (Plan + Build) and replaces code that was previously scattered across 7+ modules

### Relationship to Existing WEPs

This WEP supersedes [WEP: Wasm Plan Phase](./wep-2026-02-03-wasm-plan-phase.md). The incremental wasm_plan approach was a stepping stone; WIR is the full architectural solution to the problems wasm_plan addressed partially.

This WEP builds on [WEP: Compiler Pipeline Refactoring](./wep-2026-01-14-compiler-pipeline-refactoring.md), which introduced TIR. WIR completes the pipeline by adding the Wasm-level IR that was deferred in that proposal.

### Migration Path

- [ ] Define `wir.rs` data structures
- [ ] Implement `wir_emit.rs` (mechanical emission from WIR)
- [ ] Implement `lower.rs` Plan sub-phase (type/function planning)
- [ ] Implement `lower.rs` Build sub-phase (TIR→WIR expression/statement lowering)
- [ ] Port optimizer passes to WIR (`wir_optimize.rs`)
- [ ] Implement `wir_expand.rs` (ValueCopy expansion)
- [ ] Remove `TirExprKind::Move` and related machinery
- [ ] Remove superseded modules (`codegen.rs`, old `lower.rs`, `wasm_plan.rs`, etc.)
- [ ] Update dump commands (`--wir`, `--wir --unparse`)
- [ ] Update `docs/compiler.md`
