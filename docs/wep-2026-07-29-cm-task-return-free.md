# WEP: Reclaiming a `task.return` Payload

Gives an async-lifted export a way to reclaim the linear memory it hands the
host. Fixes wado-lang/wado#1708 — D2 of
[`post-return` for Synchronously-Lifted Exports](./wep-2026-07-28-cm-post-return.md).

## Context

An `async` export returns its value through `task.return`, not through core
function results. When that value is memory-backed — `string`, `list`, a
composite holding one — the guest allocates the payload, passes the pointer to
`task.return`, and then drops it on the floor.

`canon_task_return` lifts eagerly: the Canonical ABI has read the whole value by
the time the builtin returns. So the guest may free the payload on the very next
instruction, and nothing did — one leaked payload per call, under the `freelist`
allocator the library world defaults to.

`post-return` cannot cover this: the option may only appear on a `canon lift`
where `async` is absent, so an async-lifted export has no post-return hook at
all. Freeing after `task.return` is the only mechanism available.

## Decision

Free the buffers straight after the `task.return` call, at both sites that emit
one: the `task return` expansion in an `export async fn` body, and the export
binding's `Result` epilogue.

### What gets freed

The same ownership model as `post-return`, read from a different place. That
walk starts from one pointer into linear memory; this one starts from the flat
CM slots the value was lowered into, where a `string` or `list` occupies a
`(ptr, len)` pair. Below the first pointer the two are the same walk — a
`list<string>`'s elements sit in memory in both cases.

Both are driven by one classifier of which parts of a type own linear memory, so
neither can drift from the other or from lowering.

Two cases the flat side has to keep straight:

- **A variant's cases share slots.** `task.return` receives a variant's payload
  in the joined slots after its discriminant, so freeing reads the discriminant
  first: an unconditional free would treat an `Err` payload's slots as the `Ok`
  buffer's `(ptr, len)`. The join may also have widened a slot past `i32`, so the
  pointer is read back through the same coercion that wrote it.
- **Handles are transferred, not owned.** `task.return` hands an `own<r>` to the
  host, so the walk must not drop one — the same rule as `post-return`.

### Verification

An async mirror of the `post-return` harness: a capped-memory engine plus N
calls returning a large payload, under `freelist`, which traps on double-free
and so fails an over-eager free as loudly as a leak. It covers a bare `string`,
a `result<string, string>` (the joined-slot case), and a `list<string>` (element
buffers out of the outer pointer's reach).

## Consequences

An async export whose result owns no linear memory — every WASI world in use
today, whose results are handles and enums — emits no extra code. `wasi:http`'s
handler is unaffected for that reason.

The frees are no-ops under `bump`, which never reclaims, as they are for
`post-return`; the same reasoning applies, and the emitted code does not depend
on the allocator.

### Not covered

`task.return` takes its result as _parameters_, so a result exceeding
`MAX_FLAT_PARAMS` (16) is passed indirectly through a memory buffer instead.
That form is not lowered — the flat parameters are emitted regardless of count,
which fails validation against the canonical single-pointer signature
(wado-lang/wado#1712). So no such buffer exists to reclaim. Whoever implements
the form inherits the memory walk already used by `post-return`, since an
indirect `task.return` result is reclaimed exactly like an indirect sync
return.

## References

- `vendor/component-model/design/mvp/CanonicalABI.md` — `canon_task_return`
- [`post-return` for Synchronously-Lifted Exports](./wep-2026-07-28-cm-post-return.md) — the synchronous counterpart, and the ownership model shared with it
