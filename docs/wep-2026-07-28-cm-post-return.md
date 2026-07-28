# WEP: `post-return` for Synchronously-Lifted Exports

Gives a sync-lifted `--lib` export a way to reclaim the linear memory it hands
the host. Fixes wado-lang/wado#1683.

## Context

A non-`async` `--lib` export is lifted synchronously. When its result is
memory-backed — `string`, `list`, or a composite holding one — the Canonical ABI
returns it indirectly: the guest allocates a buffer, lowers the value into it,
and hands the host a pointer.

The ABI's only channel for telling the guest the host has finished reading is the
`post-return` option of `canon lift`:

> the optional `post-return` function … is called, passing the same core wasm
> results as parameters so that the `post-return` function can free any
> associated allocations.
>
> — `CanonicalABI.md`, `canon_lift`

Wado never emitted it, so nothing freed the payload. The library world defaults
to the `freelist` allocator precisely so that a long-running host reclaims
memory, and the return buffer was the one allocation it could not reclaim: an
export returning 1 MiB exhausts a 12 MiB memory cap after eight calls, growing by
exactly one payload per call.

`post-return` is illegal alongside `async`, so async-lifted exports — the WASI
worlds and `export async fn` — are outside this design by construction. Their
equivalent leak is D2 below.

## Decision

Emit `post-return` on a synchronous lift whose result owns linear memory, and
synthesize the function it names.

### What gets freed

`post-return` receives only the outer pointer, so reclamation is a recursive,
type-driven walk of the value in linear memory: a `list<string>` owns its element
array _and_ one payload per element. The walk covers strings, lists, records,
tuples, variants, options and results, and ends by releasing the out-pointer
buffer itself.

Two rules shape it.

Handles are never touched. Lifting _transfers_ an `own<r>` to the host, so
dropping one here would be a double-drop. Handles count as owning nothing — the
distinction is "owns no memory", not "is not four bytes wide".

A part that owns no memory produces no code, so a scalar-only result contributes
nothing beyond the outer free, and an export that owns nothing at all gets no
`post-return` option.

### Staying in step with lowering

The freeing side and the lowering side must agree on where every buffer sits.
They share one source of layout truth — the Canonical ABI layout helpers and the
type registry — rather than each deriving offsets independently.

They are separate walks even so, because lowering needs naming and type identity
that reclamation does not, and folding those in would only make the ownership
model worse at its one job. Divergence is instead made loud: a type that reaches
the CM boundary with no ownership rule fails on its first round-trip rather than
leaking silently. Silence was the old behavior, and it is what let this bug
survive.

### Verification

The two reclaiming allocators fail loudly in opposite directions, so between them
they pin both halves of correctness:

- `freelist` traps on a double-free, so a walk that visits a buffer twice cannot
  pass.
- `debug` poisons freed memory and never reuses it, so a free that runs too early
  or covers too much corrupts data the test reads back.

The regression tests are a memory-capped reproduction of the leak, an assertion
that the canonical option appears only where something is owned, and the CM type
catalog round-tripped twice under `freelist` — the widest shape corpus available,
covering strings, nested lists, tuples, options, results, records, variants,
flags, enums and newtypes.

## Consequences

An export whose result owns no memory is unaffected: its component is unchanged,
and no code size is spent where there is nothing to free. An export that does own
memory pays one extra core function and one call per invocation.

Under `bump` the emitted frees are no-ops, since that allocator never reclaims, so
a `--lib --allocator bump` build pays the call cost for no benefit. Making the
option conditional on the allocator was rejected: `canon lift` states the
component's contract with its host, and that contract should not depend on an
allocation strategy.

The lift path keeps its own, different discipline — each lift site frees what it
just read. Reclamation here is standalone, freeing a value nothing has consumed,
so the two cannot simply be merged without double-freeing. Unifying them is a
follow-up, and the `freelist` double-free trap is what would make it safe.

### Adjacent leaks

Two leaks of the same family surfaced while auditing the boundary. Neither is
fixed by `post-return`.

D1 — a top-level `string` parameter was never freed. The caller lowers it into
guest memory using the guest's own allocator, so the buffer belongs to the guest
once it has been copied onto the GC heap. Nested strings were already released by
the lift sites that read them; only the top-level case had no such site. Fixed
with this WEP, with its own reproduction.

D2 — a memory-backed `task.return` result is never freed. `task.return` lifts
eagerly, so the guest may release the payload as soon as the call returns, but
nothing does. `post-return` is illegal with `async`, and the value lives in flat
slots rather than in memory, so it needs a different fold over the same ownership
model. Filed as wado-lang/wado#1708.

## References

- `vendor/component-model/design/mvp/CanonicalABI.md` — `canon_lift`, `canon_task_return`
- `vendor/component-model/design/mvp/Explainer.md:1362-1368` — `post-return` rules
- [Async Canonical Options for `stream.read` / `stream.write`](./wep-2026-07-25-async-stream-canonical.md) — the neighbouring canonical-option audit
