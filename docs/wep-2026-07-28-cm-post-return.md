# WEP: `post-return` for Synchronously-Lifted Exports

Gives a sync-lifted `--lib` export a way to reclaim the linear memory it hands
the host, by emitting the `post-return` canonical option and synthesizing the
free function it names. Fixes wado-lang/wado#1683.

## Context

A non-`async` `--lib` export is lifted synchronously (`WorldExportPlan::sync_lift`).
When its result flattens to more than one core value — `string`, `list`, a record,
a tuple, an option, a result — the Canonical ABI returns it indirectly: the guest
`realloc`s a buffer, lowers the value into it, and returns the pointer.

```wat
(func $lib.wado/__cm_export__chunk (param i32) (result i32)
  ...
  (local.set 2 (call $realloc (i32.const 0) (i32.const 0) (i32.const 4) (i32.const 8)))
  ...)
(func $chunk (canon lift (core func $chunk-core) (memory $memory) (realloc $realloc)))
```

Two allocations per call — the 8-byte `(ptr, len)` buffer and the UTF-8 payload
behind it — and no `post-return` on the `canon lift`, which is the ABI's only
channel for telling the guest the host is done reading:

> the optional `post-return` function … is called, passing the same core wasm
> results as parameters so that the `post-return` function can free any
> associated allocations.
>
> — `CanonicalABI.md`, `canon_lift`

Nothing frees them. The library world defaults to the `freelist` allocator
precisely so a long-running host reclaims memory, and the return buffer is the
one allocation it cannot reclaim: `tests/lib_sync_lift_post_return.rs` traps on
call 8 of 48, having grown linear memory by exactly one payload per call.

`post-return` is illegal alongside `async`, so async-lifted exports (the WASI
worlds, `export async fn`) are outside this design by construction. Their
equivalent leak is D2 under [Adjacent leaks](#adjacent-leaks).

## Decision

### `CmShape` — one classifier for lower and free

The hard part is not calling `realloc`; it is knowing what to free. `post-return`
receives only the outer pointer, so the free must walk the CM-laid-out value in
linear memory and reach every nested buffer: a `list<string>` owns its element
array _and_ one payload per element.

The lowering side already knows this shape. `synthesize_lower_wasi_type_to_memory`
dispatches a Wado type to record / variant / tuple / option / list / primitive and
computes each part's offset through `cm_abi::layout_*_with_registry_scoped`. A free
walker that re-derived the same dispatch independently would rot the moment a type
is added on one side only.

So the dispatch decision is extracted into a single classifier, `cm_shape(ty, …)`,
and both walkers match on it exhaustively:

```rust
enum CmShape {
    /// Fixed-width: ints, floats, bool, char, enum, flags, resource handle.
    /// Owns nothing.
    Scalar,
    /// `(ptr, len)` at addr; owns `len` bytes at align 1.
    Str,
    /// `(ptr, len)` at addr; owns `len * elem.size` bytes at `elem.align`,
    /// plus whatever each element owns.
    List { elem: Box<CmField> },
    /// Record or tuple: fields at fixed offsets.
    Record { fields: Vec<CmField> },
    /// Variant, option or result: a discriminant plus one payload per case
    /// at a shared offset.
    Variant { disc: DiscWidth, payload_offset: u32, cases: Vec<Option<CmField>> },
}

struct CmField { ty: Type, offset: u32, size: u32, align: u32 }
```

`CmField` carries the layout facts straight from `cm_abi`, so there is one source
of truth for offsets. A new CM type kind means a new `CmShape` case, which fails
to compile in both matches — the workspace already forbids wildcard arms, so this
is enforced, not conventional.

The per-arm lowering bodies are unchanged; only the classification head moves.
The refactor is output-preserving and the golden corpus proves it.

### The free walk

`synthesize_free_cm_value(shape, addr)` folds over `CmShape`:

| Shape     | Emitted                                                                                   |
| --------- | ----------------------------------------------------------------------------------------- |
| `Scalar`  | nothing                                                                                   |
| `Str`     | `realloc(load(addr), load(addr+4), 1, 0)`                                                 |
| `List`    | per-element walk when the element owns memory, then `realloc(base, count*size, align, 0)` |
| `Record`  | walk each field that owns memory, at its offset                                           |
| `Variant` | load the discriminant, walk the active case's payload when it owns memory                 |

Two properties carry the correctness.

Handles are never touched. An `own<r>` in a lifted result was transferred to the
host during lift; `resource.drop` here would be a double-drop. Handles classify as
`Scalar`, which emits nothing — that is why `Scalar` must mean "owns nothing", not
"four bytes".

A branch that owns no memory emits no code. `owns_memory` is a recursive predicate
over `CmShape`; a `[u32, u32]` return produces only the outer free.

The list guard `count > 0` mirrors the one `synthesize_lift_list` already carries,
and keeps the `debug` allocator's poison length exact.

### Wiring

1. Synthesis (`synthesis/cm_binding.rs`, `SyncReturn` path). When the world return
   type owns memory, synthesize `__cm_post_return__{export}`: `(param i32) -> ()`,
   body = the free walk over the return type at the pointer, then
   `realloc(ptr, cm_size(ty), cm_align(ty), 0)` for the outptr buffer itself.
   Marked `is_cm_export` so DCE keeps it. Recorded in
   `Package::post_return_binding_names`, alongside `export_binding_names`.

2. Plan (`wir_build/component_plan.rs`). `WorldExportPlan` gains
   `post_return_core_name: Option<String>`, only ever `Some` when `sync_lift`.

3. WIR (`wir_build/functions.rs::register_exports`). Export the post-return
   function from the core module under that name.

4. Codegen (`codegen/component.rs`). Alias the core function and push
   `CanonicalOption::PostReturn(idx)` into `lift_opts`.

Owning memory implies more than one flat result, which implies the indirect
return — so the post-return parameter is always the single `i32` outptr. The
converse would be a Canonical ABI contradiction rather than user input, so
synthesis asserts it instead of branching on it.

### Verification

The two allocators make the failure modes loud, in opposite directions:

- `freelist` traps on double-free (`fl_used(header) == 0 → unreachable`). A free
  walk that visits a buffer twice cannot pass.
- `debug` poisons freed memory with `0xFF` and never reuses it. A free that runs
  too early, or with an oversized length, corrupts data the test then reads back.

Tests:

- [ ] `tests/lib_sync_lift_post_return.rs` — drop both `#[ignore]`s. 48 calls
      returning 1 MiB each under a 12 MiB cap; flat memory is the assertion.
- [ ] `tests/cm_catalog.rs` — the type catalog (40+ exports: strings, nested
      lists, tuples, options, results, records, variants, newtypes) already runs
      under `debug`. Add a `freelist` pass calling each export twice, so the
      double-free guard covers every shape the free walk can take.
- [ ] A WAT assertion that `(post-return …)` is present for a `String`-returning
      lib export and absent for a scalar-returning one.
- [ ] Golden fixture regeneration for the CM catalog components.

## Consequences

An export whose result owns no memory gets no `post-return` at all: its component
is byte-identical to today's, and no wasm size is spent where there is nothing to
free. An export that does own memory pays one extra core function and one call per
invocation.

Under `bump` the emitted frees are no-ops — the allocator never reclaims — so a
`--lib --allocator bump` build pays the call cost for no benefit. That is the
allocator's documented trade-off, not a reason to make the option conditional on
it: `canon lift` is fixed at compile time and the allocator choice is already
visible there, but branching the ABI on it would make the component's contract
depend on an allocation strategy.

The lift path keeps its own, different free discipline: each lift site frees what
it just read, and `synthesize_free_element` covers only the leaves the recursion
does not self-free. Re-pointing those call sites at the standalone walker would
free the same buffers twice. They are left alone here; unifying them is a follow-up
whose safety rests on the same `freelist` double-free guard.

### Adjacent leaks

Two leaks of the same family surfaced while reading the boundary. Both are
verified from generated WAT, and neither is fixed by `post-return`.

D1 — a top-level `String` parameter is never freed. The host lowers it into guest
memory with the guest's `realloc`; the adapter copies it into a GC string and drops
the pointer:

```wat
(func $lib.wado/__cm_export__take (param i32 i32) (result i32)
  (call $lib.wado/take (call $core:rt/memory_to_gc_string (local.get 0) (local.get 1))))
```

Nested strings are already freed — `synthesize_lift_list` and
`synthesize_lift_option_inner` free what they read — so this is the top-level arm
of `synthesize_lift_from_flat_params` alone, and it affects async-lifted exports
equally. One-arm fix with its own reproduction fixture, landed with this WEP.

D2 — a memory-backed `task.return` result is never freed. `canon_task_return`
lifts eagerly (`lift_flat_values` before it returns), so the guest may free
immediately after the call. Nothing does: `push_result_task_return_epilogue`
lowers a `String` through `cm_lower_string` and abandons the payload.
`post-return` is illegal with `async`, and the value lives in flat slots rather
than in memory, so the fix is a different fold over the same `CmShape`. Separate
issue, separate reproduction.

## References

- `vendor/component-model/design/mvp/CanonicalABI.md` — `canon_lift`, `canon_task_return`
- `vendor/component-model/design/mvp/Explainer.md:1362-1368` — `post-return` rules
- [Async Canonical Options for `stream.read` / `stream.write`](./wep-2026-07-25-async-stream-canonical.md) — the neighbouring canonical-option audit
