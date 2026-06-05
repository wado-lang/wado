# WEP: NIR Skeleton Arena (Layer 1)

## Context

[Worklist-Driven NIR Rewrite Engine](./wep-2026-06-05-worklist-rewrite-engine.md)
fixed the substrate as two layers and put Layer 1 — the effect skeleton —
first, as the NIR representation itself. This WEP specifies Layer 1's data
structures: the `NodeId` spaces, how parent and use edges are stored and kept
coherent under rewrites, and the seam left for Layer 2. The worklist engine and
its rule / fairness / conflict policy stay in the follow-up engine WEP; this
WEP only builds the substrate the engine stands on.

Today a function body is an owned tree: `NirFunction.body: Option<NirBlock>`,
`NirBlock { stmts: Vec<NirStmt> }`, expressions nested through `Box<NirExpr>`
(`nir.rs`). Two properties the engine needs are absent:

- No stable handle. `&mut NirExpr` recursion owns the path to a node, so a
  worklist cannot hold a node across an edit.
- No parent or use edges. There is no way to go from a node to its parent, or
  from a local's definition to its reads.

CSE already pays for the missing structural handle: it hand-rolls a `CseKey`
over three node kinds (`cse.rs`) because NIR offers nothing better.

## Decision

A function body becomes a per-function arena, `Body`, which is the canonical
post-lower form (not a structure built on the side — see Migration). Closure
`__call` methods and non-trivial global initializers are bodies too and each
own a `Body`.

### Id spaces

Typed ids, one arena (a `PrimaryMap`-style `Vec`) per category:

- `ExprId`, `StmtId`, `BlockId`, `PatId` — newtype indices over `u32`.
- `NodeRef = Expr(ExprId) | Stmt(StmtId) | Block(BlockId) | Pat(PatId)` — the
  uniform handle the worklist and the parent map speak.

Typed rather than one uniform id so rule signatures read `fn(&mut Body, ExprId)`
and the compiler rejects passing a statement where an expression is expected;
the large majority of rules are expr-typed. Records that are never independent
rewrite targets — match arms, struct fields, call args — are not id-bearing.
They live inline in their parent node as small structs holding child ids
(`ArmData { pattern: PatId, guard: Option<ExprId>, body: ExprId, span }`). The
parent of an arm's body is the enclosing `Match` expr; arms / fields / args are
transparent to the parent map.

### Node payloads

```
ExprNode  { kind: ExprKind, type_id: TypeId, span: Span }
StmtNode  { kind: StmtKind, span: Span }
BlockNode { stmts: Vec<StmtId>, span: Span }
PatNode   { kind: PatKind, span: Span }
```

`ExprKind` mirrors `NirExprKind` with every child reference rewritten:
`Box<NirExpr>` → `ExprId`, `Vec<NirExpr>` → `Vec<ExprId>`, `NirBlock` →
`BlockId`, `CallArg { expr, is_mut }` → `{ expr: ExprId, is_mut }`. It is a
parallel enum, following the repo's TIR / NIR / WIR "separate enums"
convention. Making `NirExprKind<R>` generic over the child reference type was
considered and rejected: the node wrapper, and the placement of `type_id` /
`span`, differ between tree and arena, so a generic over the leaf reference
would not actually unify the two forms.

`type_id` and `span` stay per node. `span` feeds diagnostics, optimizer
remarks, and DWARF, and is part of skeleton identity — Layer 2 is the
span-free layer, not this one.

Variable-arity children are inline `Vec<ExprId>` / `Vec<StmtId>` to start. A
`ListPool` (compact, splice-cheap child lists — the Cranelift `EntityList`
shape) is the documented optimization if `Vec` churn shows up in profiles; it
changes the storage, not the node-facing API.

### Parent edges

`parent: NodeRef -> Option<NodeRef>`, stored as one `Vec<Option<NodeRef>>` per
category, indexed by the raw id. Maintained incrementally by the edit API,
never recomputed: every operation that points a slot at a child also sets that
child's parent. Parent is the nearest id-bearing ancestor; the body root's
parent is `None`. This is the edge the worklist follows to re-enqueue a node's
context after it is rewritten.

### Use index

Locals are function-scoped (`Local { index }`), so the use index lives on the
`Body`:

```
local_uses: index -> { def: Option<StmtId>, reads: Vec<ExprId>, writes: Vec<ExprId> }
```

`reads` are `Local` expr nodes; `writes` are the place forms — `Assign { target:
Local }`, `Local.field = …`, `&local` / `&mut local`. `def` is the binding
`Let` / `LetDestructure` where one exists (params have none). Maintained
incrementally: allocating or freeing a node that mentions a local updates the
index. The engine reads it to re-enqueue a local's uses when its definition
changes — copy propagation, const-fold through a `Let`, store-to-load
forwarding.

Function-level alias facts that gate soundness for many passes —
`address_taken_locals`, `stores_aliased_locals`, and the `locals: Vec<NirLocal>`
table — travel on `Body` beside the arena, mirrored from `NirFunction`.

### Edit API and the engine seam

Rules do not touch the category `Vec`s directly; they go through an edit API
that keeps the parent and use indices coherent and records what to re-enqueue:

- `alloc_expr(kind, type_id, span) -> ExprId` (and `_stmt` / `_block` / `_pat`).
- `replace_kind(id, new_kind)` — rewrite a node in place. The id is stable, so
  worklist entries and parent links survive the edit. The op diffs the old and
  new child / local sets and fixes both indices.
- `set_child(parent, slot, new)` and `splice(block, range, stmts)` — structural
  edits for operand slots and statement lists.

Stable ids under in-place replacement is exactly what the tree cannot give: the
worklist holds `NodeRef`s, a rule rewrites in place, and the handle stays valid.
Every edit enqueues the affected `parent[...]` and, when a local's def / use set
changed, the neighbouring def / use nodes. The queue itself, fairness, and the
rule-conflict policy are the engine WEP's concern, not this one.

Nodes are not freed mid-run. Liveness is reachability from `root`; an optional
compaction pass (or the arena → tree lowering during migration) reclaims
orphaned subtrees, consistent with DCE being a separate stage around the engine.

### Seam for Layer 2

Layer 1 leaves pure expressions as ordinary `ExprNode`s. When Layer 2 (the
hash-consed value e-graph) is built, a referentially-transparent `ExprId` —
classified by `optimize/mod_ref.rs`, not by node kind — is represented by a
value-graph id, and skeleton operands widen to `Operand = Skel(ExprId) |
Value(ValueId)`. Nothing in Layer 1's id spaces, parent map, or use index has
to change shape for that; the seam is the operand type, introduced later. This
WEP does not build it, and whether it is built at all is re-decided after the
engine lands (see the engine WEP's sequencing).

## Migration

The arena is the canonical representation; the converters below are temporary
scaffolding, not a permanent boundary. This is the distinction the engine WEP
drew when it rejected a conversion boundary: that design kept the tree as the
representation and paid tree ↔ arena translation on every engine run forever.
Here the converters exist only to land the arena incrementally and are deleted
once the producer and consumer speak arena.

Each phase keeps the full e2e suite and golden WIR fixtures green
(`tests/generated/fixtures/*.wir.wado`), satisfying the engine WEP's "codegen
must not regress" requirement.

- [x] Phase 1 — `Body`, the id spaces, the node payloads, and the converters
      (`nir_arena.rs`); the parent map, use index, and the mutating edit API
      are deferred to Phase 4 as planned. The tree stays canonical; the arena
      is built on the side. Green check: a `tree → arena → tree` round-trip at
      the optimize entry (`WADO_TRACE=arena_roundtrip`) keeps the full e2e
      suite bit-identical — 2786 passed, 0 failed at O0/O2.
      Phase 2 and Phase 3 were reordered (wir_build before lower) after Phase 1.
      `NirFunction.body` is a single-typed field, so the arena becomes the _canonical_
      body only once its largest mutator — `optimize` — is ported (Phase 4). Until
      then the arena must be bracketed by a converter, and the cleanest place to put
      that converter is at the entry of a read-only consumer. `wir_build` is exactly
      that: porting it first (a `tree → arena` build at its entry, then arena reads
      throughout) needs no change to `lower` or `optimize` and leaves the tree
      canonical, while validating that the arena losslessly carries everything codegen
      needs — the strongest possible check on the representation. Porting `lower`
      first would instead need an `arena → tree` step at its exit (overhead) because
      `optimize` still reads the tree.

- [ ] Phase 2 — port `wir_build` to read `Body`, built by a `tree → arena`
      converter at its entry. `lower` and `optimize` are untouched; the tree
      stays canonical through them. Green check: WIR / Wasm output unchanged
      across the e2e and golden suites.
      - In progress (WIP, non-compiling). The port is one atomic unit: the
        `FunctionTranslator` body-translation call graph is mutually recursive
        across `translate.rs`, `pattern_match.rs`, `calls.rs`,
        `primitive_ops.rs`, and the body-shape helpers in `canonical_abi.rs`,
        with `translate_expr` the chokepoint — so there is no green checkpoint
        until every signature flips from `&NirExpr` / `&NirStmt` / `&NirBlock` /
        `&NirPattern` to `ExprId` / `StmtId` / `BlockId` / `PatId`.
      - Technique: `FunctionTranslator` gains `body: &'a Body` (built once per
        function at the entry via `Body::from_block`). Each method takes ids and
        reads nodes via `let arena = self.body; let node = &arena.<map>[id];`
        (copying the `&'a Body` out so it does not borrow `&mut self`). Match
        arms already renamed (`NirExprKind` → `ExprKind`, etc., identical
        variant/field names); child fields are now `ExprId` (Copy), so
        recursion is `self.translate_expr(*child)`. `functions.rs`'s global-init
        path (`translate_global_init`) stays on the tree — globals are not part
        of the `FunctionTranslator` cluster.
      - Done so far: entry wired (arena built, threaded, `translate_block(root)`);
        enum renames + arena imports in the cluster files; `translate_block`,
        `declare_locals_from_stmts`, `translate_stmts` ported.
      - Remaining: `translate_stmt`, `translate_expr*`, the value-position
        helpers, all of `pattern_match.rs`, and the `calls.rs` /
        `primitive_ops.rs` / `canonical_abi.rs` arg / shape helpers.
- [ ] Phase 3 — port `lower` to emit `Body` directly; `optimize` is ported
      alongside / before so the arena flows lower → optimize → wir_build with
      no converter. The arena is now the only post-lower body form.
- [ ] Phase 4 — port the peephole passes onto the edit API. This is where the
      engine WEP begins. Flow-sensitive passes (`field_scalarize`, `licm`,
      `tmpl_hoist`, `value_copy_demote`, `store_load_forward`) keep their own
      walkers but read the arena.
- [ ] Phase 5 — delete the tree enums (`NirExprKind` / `NirStmtKind` /
      `NirBlock` / `NirPattern` as owned trees) once no consumer remains.

## Phase 1 implementation notes

Decisions pinned before coding, so Phase 1 is mechanical:

- Substrate crate. Built on `cranelift-entity` (already in the tree at the
  vendored `0.133` generation via `cranelift-codegen`), promoted to a direct
  dependency: `entity_impl!` for the id newtypes, `PrimaryMap<Id, Node>` per
  category, `EntityList` + `ListPool` available for variable-arity children
  when `Vec` churn warrants it. It is pure data structures and builds on
  `wasm32-unknown-unknown` (the crate's CI target); `mise run check-deps` is
  updated to pin it to the same generation as the other vendored cranelift /
  wasm-tools crates.
- `Body` shape. `NirFunction.body: Option<NirBlock>` is joined (Phase 1) by an
  out-of-band `Body` holding `exprs / stmts / blocks / pats: PrimaryMap<…>`, a
  `root: BlockId`, and the function-level facts the later passes read beside
  the arena (`locals`, `address_taken_locals`, `stores_aliased_locals`).
  Signature metadata (params, return, effects, kind, …) stays on
  `NirFunction`. The parent map and per-`Body` use index are added in Phase 4.
- Phase 1 oracle. NIR's tree carries no `PartialEq`, so equivalence is not
  checked structurally on the IR. Instead the round-trip is validated through
  the existing safety net: a `tree → Body → tree` pass spliced in at the
  optimize entry must leave every e2e and golden WIR fixture bit-identical.
  This reuses the golden / wasm comparison already in CI rather than growing a
  new `PartialEq` over ~40 expr kinds.
- Variant mapping. `ExprKind` / `StmtKind` / `PatKind` are transcribed
  field-for-field from `NirExprKind` / `NirStmtKind` / `NirPattern` with child
  references rewritten to ids; the one-to-one table is the converter's body and
  is the single place the two enums are kept in lockstep.

## Consequences

### Benefits

- The worklist becomes possible: stable handles, parent edges, and a local use
  index are the substrate the engine requires, now present.
- CSE's hand-rolled `CseKey` and similar per-pass structural keys can be
  retired in favour of one shared handle.
- The Layer 2 seam is reserved without committing to Layer 2.

### Trade-offs

- A parallel `ExprKind` / `StmtKind` / `PatKind` enum exists alongside the tree
  enums until Phase 5. The duplication is mechanical and bounded, and is paid
  down when the tree enums are deleted.
- The edit API must be the only mutation path during a run; a pass that pokes
  the `Vec`s directly can desynchronise the parent / use indices. Enforced by
  keeping the fields private to `Body` and exposing only the API.
- One more lowering (tree ↔ arena) lives in the tree during Phases 1–3, then is
  removed.

## See also

- [Worklist-Driven NIR Rewrite Engine](./wep-2026-06-05-worklist-rewrite-engine.md)
  — the two-layer substrate decision and sequencing this WEP implements.
- [Normalized IR (NIR) Layer](./wep-2026-05-11-nir.md) — the tree-shaped body
  IR this arena re-bases.
- `optimize/mod_ref.rs` — the referential-transparency check that will gate the
  Layer 1 / Layer 2 boundary.
- `optimize/cse.rs` — the hand-rolled `CseKey` the shared handle replaces.
