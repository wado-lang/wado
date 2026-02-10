# WEP: WIR (Wasm IR) Layer

## Context

`codegen.rs` is 11,775 lines and conflates three concerns:

1. **Planning**: Deciding Wasm GC type layouts, assigning func/type/local indices, resolving call targets
2. **Lowering**: Translating TIR semantics to Wasm GC operations (value copy, Option as nullable ref, variant as tagged struct)
3. **Emission**: Encoding `wasm_encoder::Instruction` into bytes

The Lower phase (8,061 lines) produces a "post-lower TIR" that is structurally very different from "pre-lower TIR" — dead nodes that panic in codegen (`IfPattern`, `Closure`, `Capture`), mutated type tables (box lowering), and synthetic types/functions. The optimizer (10,547 lines) runs on this post-lower TIR, but several passes exist solely to bridge TIR and codegen (`Move` nodes, `needed_copy_types`, name-resolution-heavy DCE).

### Problems

- **No place for Wasm-level optimization**: Peephole patterns like `StructGet(StructNew(fields), i) → fields[i]` cannot be expressed at TIR level or codegen level
- **Dead TIR nodes**: Several `TirExprKind` variants panic in codegen
- **codegen does analysis**: Type registration, `type_id_to_valtype`, value copy generation, call resolution — not emission
- **Optimizer bridges TIR and codegen**: `Move` nodes, `needed_copy_types`, scratch local analysis pollute TIR

## Decision

Introduce **WIR (Wasm IR)**, a tree-structured IR with resolved Wasm indices, as the boundary between language semantics (TIR) and Wasm operations. Lower produces WIR directly from TIR. The optimizer moves from TIR to WIR.

### Pipeline

```
Source → ... → Resolve → Effect Check → Monomorphize
→ Lower (TIR→WIR) → Optimize (WIR→WIR) → Expand → Emit → Wasm bytes
```

Everything before Lower works with TIR (language semantics). Everything after works with WIR (Wasm operations).

### Design Principles

- **WIR nodes map 1:1 to Wasm GC instructions, in tree form.** The single exception is `ValueCopy`, an explicit expansion point. Emit is purely mechanical (tree flatten, zero decisions). Peephole optimization is natural pattern matching.
- **All indices are resolved.** Type, function, local, global indices and data segment offsets — all resolved during Lower. No lookups in Optimize or Emit.
- **Statement/Expression split mirrors Wasm semantics.** `WirExpr` produces exactly one value on the stack. `WirStmt` has side effects only.

### WIR Data Structures

```rust
struct WirComponent {
    main_module: WirModule,
    memory_module: WirMemoryModule,
    bundled_modules: Vec<BundledModule>,  // opaque pre-compiled blobs
    component: WirComponentStructure,     // declarative CM structure
}

struct WirModule {
    types: Vec<WirTypeDef>,     // GC struct/array, function types
    imports: Vec<WirImport>,
    functions: Vec<WirFunction>,
    tables: Vec<WirTable>,
    globals: Vec<WirGlobal>,
    data: Vec<u8>,              // concatenated string literal bytes
    elements: Vec<WirElement>,
    exports: Vec<WirExport>,
    names: WirNames,
}

struct WirFunction {
    type_idx: u32,
    locals: Vec<WirLocal>,
    body: WirBody,
    name: Option<String>,
    branch_hints: Vec<WirBranchHint>,  // attached at Lower time
    metadata: WirFunctionMetadata,     // for optimizer (is_pure, is_recursive, etc.)
}
```

#### WirStmt

```rust
enum WirStmt {
    // Stores
    LocalSet { idx: u32, value: WirExpr },
    GlobalSet { idx: u32, value: WirExpr },
    StructSet { type_idx: u32, field_idx: u32, obj: WirExpr, value: WirExpr },
    ArraySet { type_idx: u32, arr: WirExpr, index: WirExpr, value: WirExpr },
    ArrayCopy { dst_type: u32, dst: WirExpr, dst_offset: WirExpr,
                src_type: u32, src: WirExpr, src_offset: WirExpr, len: WirExpr },
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
    Exec(WirExpr),
}
```

#### WirExpr

```rust
enum WirExpr {
    I32Const(i32), I64Const(i64), F32Const(f32), F64Const(f64),

    LocalGet { idx: u32 },
    LocalTee { idx: u32, value: Box<WirExpr> },
    GlobalGet { idx: u32 },

    // op is wasm_encoder::Instruction (reused, not re-defined)
    BinOp { op: Instruction, lhs: Box<WirExpr>, rhs: Box<WirExpr> },
    UnOp { op: Instruction, operand: Box<WirExpr> },
    Convert { op: Instruction, operand: Box<WirExpr> },

    StructNew { type_idx: u32, fields: Vec<WirExpr> },
    StructGet { type_idx: u32, field_idx: u32, obj: Box<WirExpr> },

    ArrayNew { type_idx: u32, size: Box<WirExpr>, init: Box<WirExpr> },
    ArrayNewDefault { type_idx: u32, size: Box<WirExpr> },
    ArrayNewData { type_idx: u32, data_idx: u32, offset: Box<WirExpr>, size: Box<WirExpr> },
    ArrayNewFixed { type_idx: u32, elements: Vec<WirExpr> },
    ArrayGet { type_idx: u32, arr: Box<WirExpr>, index: Box<WirExpr> },
    ArrayGetS { type_idx: u32, arr: Box<WirExpr>, index: Box<WirExpr> },
    ArrayGetU { type_idx: u32, arr: Box<WirExpr>, index: Box<WirExpr> },
    ArrayLen { arr: Box<WirExpr> },

    RefNull { heap_type: HeapType },
    RefIsNull { expr: Box<WirExpr> },
    RefAsNonNull { expr: Box<WirExpr> },
    RefCast { type_idx: u32, nullable: bool, expr: Box<WirExpr> },
    RefTest { type_idx: u32, nullable: bool, expr: Box<WirExpr> },
    RefEq { lhs: Box<WirExpr>, rhs: Box<WirExpr> },
    RefFunc { func_idx: u32 },

    Call { func_idx: u32, args: Vec<WirExpr> },
    CallRef { type_idx: u32, args: Vec<WirExpr>, func_ref: Box<WirExpr> },

    Load { op: Instruction, addr: Box<WirExpr> },

    If { result_type: BlockType, cond: Box<WirExpr>, then_expr: Box<WirExpr>, else_expr: Box<WirExpr> },
    Block { result_type: BlockType, body: Vec<WirStmt>, result: Box<WirExpr> },
    Select { val_type: ValType, cond: Box<WirExpr>, if_true: Box<WirExpr>, if_false: Box<WirExpr> },

    /// Expansion point — expanded to Wasm GC ops by Expand phase before Emit.
    /// Kept as high-level node for debuggability and copy elimination.
    ValueCopy { kind: CopyKind, source: Box<WirExpr> },
}
```

### Lower (TIR → WIR)

Lower is the single TIR→WIR conversion point. It has two sub-phases:

- **Plan**: Collect types and functions, assign all indices, build data segments, plan CM boundary
- **Build**: Per function — allocate locals, lower TIR body to WIR body (pattern desugaring, closure/box/variant lowering, ValueCopy insertion, all in one pass)

Lower absorbs current `lower.rs`, `wasm_plan.rs`, `component_model.rs`, `wasm_builder.rs`, and the analysis/planning parts of `codegen.rs`. CM adapter functions (HTTP handler return, effect wrappers) are generated as regular WIR function bodies, enabling future auto-generation.

### Optimizer on WIR

The optimizer moves from TIR to WIR. Key changes:

| Pass       | Change                                                                               |
| ---------- | ------------------------------------------------------------------------------------ |
| inline     | Simpler: `func_idx` direct lookup instead of name resolution                         |
| ref_elim   | Subsumed by GC allocation elimination: `StructGet(StructNew(fields), i) → fields[i]` |
| copy_prop  | Standard `LocalSet`/`LocalGet` propagation                                           |
| const_fold | Simpler: Wasm opcode encodes type, no newtype traversal                              |
| licm       | Similar: `Loop` + `StructGet` hoisting                                               |
| rewrite    | Select lowering only. Move insertion and value-copy-types eliminated                 |
| dce        | Trivial: `func_idx` reachability instead of 500+ lines of name resolution            |

New optimizations enabled by WIR's tree structure:

- **GC allocation elimination**: `StructGet(StructNew(fields), i) → fields[i]` — subsumes ref_elim, value copy elim, tuple elim
- **Null check elimination**: `RefIsNull(StructNew(..)) → I32Const(0)`
- **Cast elimination**: `RefCast(T, StructNew(T, ..)) → StructNew(T, ..)`
- **ValueCopy elimination**: `ValueCopy(StructNew(..)) → StructNew(..)` (fresh values need no copy)
- **Constant folding**: `BinOp(I32Add, I32Const(a), I32Const(b)) → I32Const(a+b)`

### TIR Move Node Elimination

`TirExprKind::Move` is eliminated. `ValueCopy` in WIR inverts the communication — instead of marking what does NOT need copy (Move), WIR marks what DOES need copy (ValueCopy). Lower inserts `ValueCopy` where needed; optimizer removes unnecessary ones. This also removes `needed_copy_types`, `copy_source_types`, and `copy_context.rs`.

## Consequences

### Benefits

- **~55% code reduction** in lower+optimize+codegen (estimated ~33,800 → ~15,150 lines)
- **Wasm-level optimization** expressible for the first time
- **Dead TIR nodes eliminated at the type level** — no `IfPattern`/`Closure`/`Capture` in WIR
- **Clean architectural boundary** — TIR = language, WIR = Wasm, no ambiguous "post-lower TIR"
- **Mechanical Emit** — ~800 lines, zero decisions, zero lookups

### Trade-offs

- **Large refactoring scope**: Lower, optimizer, and codegen are rewritten
- **Optimizer loses named identifiers**: Mitigated by optional `name` fields on WIR nodes
- **Lower becomes the largest single pass**: ~8,500 lines, but with clear Plan/Build internal structure

### Relationship to Existing WEPs

This WEP supersedes [WEP: Wasm Plan Phase](./wep-2026-02-03-wasm-plan-phase.md). It builds on [WEP: Compiler Pipeline Refactoring](./wep-2026-01-14-compiler-pipeline-refactoring.md), completing the pipeline with the Wasm-level IR deferred in that proposal.
