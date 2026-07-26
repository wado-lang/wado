# WEP: Async Canonical Options for `stream.read` / `stream.write`

Makes the stream halves of [ReturnCode Semantics](./wep-2026-03-01-cm-resource-canonical-attrs.md)
in WEP-2026-03-01 true, by lowering both stream copy canonicals with `async`.

## Context

WEP-2026-03-01 specified one BLOCKED protocol for all four CM copy operations:

| CM ReturnCode | Wado behavior                     |
| ------------- | --------------------------------- |
| BLOCKED       | Wait via waitable-set, then retry |

`core:rt` carries two helpers for it — `future_await_blocked` for futures and
`wait_for_blocked` for streams. Only the future one works.

### Streams are lowered synchronously

`codegen/component.rs` emits the two families with different canonical options:

- `future.read` / `future.write` — `CanonicalOption::Async`
- `stream.read` / `stream.write` — no `Async`

Without `async`, the canonical never hands BLOCKED back to the guest; it suspends
the calling thread until the copy completes (`CanonicalABI.md`, `stream_copy`):

```python
if not e.has_pending_event():
    if not opts.async_:
      e.wait_for_pending_event()
    else:
      return [BLOCKED]
```

So the `result == BLOCKED` branch that synthesis emits around every stream read
and write, and the `wait_for_blocked` helper behind it, are unreachable by
construction.

### The unreachable path is also wrong

`wait_for_blocked` joins the stream handle to a fresh waitable set and drops the
set without unjoining. `waitable-set.drop` deletes the set through
`ResourceTable::delete`, which rejects an entry that still has children, so the
helper traps with `resource has children` on any path that reaches it.
`future_await_blocked` avoids this with a `waitable_join(handle, 0)` before the
drop; `wait_for_blocked` never got that line.

The spec rejects the shape independently. `stream_copy` traps on a synchronous
copy against a stream end that sits in a waitable set, and the synchronous wait
asserts the same precondition:

```python
trap_if(e.in_waitable_set() and not opts.async_)

def wait_for_pending_event(self):
    assert(not self.in_waitable_set() and not self.has_sync_waiter)
```

A synchronous stream can therefore never be waited on through a waitable set,
which is exactly what `wait_for_blocked` attempts.

### `Stream::cancel_read` / `cancel_write` can only trap

`cancel_copy` traps unless the end is currently `COPYING`:

```python
trap_if(e.state != CopyState.COPYING or e.has_sync_waiter)
```

A synchronous copy completes before the guest regains control, so the guest can
never observe `COPYING`. The two methods are declared in `prelude/types.wado`,
emitted with `async = false`, called from nowhere, and would trap if they were.
Asynchronous copies do not change this on their own — see "Cancellation stays
unreachable".

### The spec baseline is async

`canon stream.{read,write}` validation gates the synchronous form, not the
asynchronous one:

```
🚝 - `async` is allowed to be omitted, otherwise it must be present
```

🚝 is "enabling more canonical ABI options on more async-related builtins", one
of the emoji-gated features not yet in a WASI Developer Preview release. Wado's
streams depend on that gate; its futures do not.

### Synchronous copies serialize the guest

A blocking read or write suspends the whole guest thread, so a component cannot
read one stream while writing another, cannot wait on a stream and a timer
together, and cannot abandon a stalled transfer. Handlers that write a response
body before `task return` deadlock until the host timeout rather than making
progress.

## Decision

Emit `CanonicalOption::Async` for `stream.read` and `stream.write`, matching
`future.read` / `future.write` and the ungated spec baseline.

### Await helper

Replace `wait_for_blocked` and `future_await_blocked` with one `core:rt` helper,
`cm_await_blocked`. The two bodies are identical apart from the unjoin, which
both need:

```wado
internal fn cm_await_blocked(handle: i32) -> i32 {
    let ws = builtin::waitable_set_new();
    builtin::waitable_join(handle, ws);
    let evt_ptr = builtin::realloc(0, 0, 4, 8);
    let code = builtin::waitable_set_wait(ws, evt_ptr);
    let status = builtin::i32_load(evt_ptr + 4);
    let freed = builtin::realloc(evt_ptr, 8, 4, 0);
    builtin::waitable_join(handle, 0);
    builtin::waitable_set_drop(ws);
    return status;
}
```

`waitable-set.wait` stays synchronous. Wado tasks are stackful and satisfy
`task_may_block`, which is what lets the existing future path wait this way.

### Waitable set reuse

Creating and dropping a waitable set per blocked operation is a per-chunk cost on
every stream transfer. A per-task set, joined and unjoined around each await,
replaces it. This is a follow-up to the correctness change, not a precondition.

### Cancellation stays unreachable

`Stream::cancel_read` / `cancel_write` remain uncallable, and this change does
not alter that. `cancel_copy` traps unless the end is `COPYING`:

```python
trap_if(e.state != CopyState.COPYING or e.has_sync_waiter)
```

The synthesized bindings await BLOCKED inline — `if result == BLOCKED { result =
cm_await_blocked(handle) }` — so control only returns to user code once the copy
has settled to `IDLE` or `DONE`. Asynchronous canonicals move where the wait
happens, not whether user code can observe a copy in flight.

Making cancel reachable is a separate change: the binding would have to return
the BLOCKED state to user code rather than absorbing it, which means a different
`Stream` API shape. Until then the two methods stay declared-but-untestable, as
they were before this WEP.

## Consequences

Positive:

- Streams and futures share one lowering, one await helper, and one set of
  semantics. The asymmetry that let `wait_for_blocked` rot undetected is gone.
- The BLOCKED branch that synthesis already emits around every stream operation
  becomes live code with test coverage, instead of dead code shipped in every
  component.
- Wado stops depending on the 🚝 gate for its core I/O path.
- Multiplexing and overlapping transfers become expressible. Cancellation does
  not — see "Cancellation stays unreachable".

Negative:

- Every blocked stream operation now costs a waitable-set round trip that the
  host previously absorbed. Unblocked copies still return their result directly,
  so the cost lands only on genuinely blocking transfers.
- `stream.read` / `stream.write` become reentrancy points: the guest yields to
  the host mid-transfer where it previously did not.

Neutral:

- No user-facing API change. `Stream::read` / `StreamWritable::write` keep their
  signatures and their DROPPED-as-EOF semantics.

## TODO

- [x] Emit `CanonicalOption::Async` for `stream.read` / `stream.write`
- [x] Replace `wait_for_blocked` and `future_await_blocked` with `cm_await_blocked`,
      which unjoins before dropping the set
- [x] E2E fixture that blocks a stream read and a stream write, covering the
      await path at both ends (`stream_await_blocked_roundtrip.wado`)
- [x] Regenerate the golden fixtures
- [ ] Reuse a per-task waitable set instead of new/drop per await

## Related WEPs

- [Redesign Wasm CM Builtins as Resource Canonical Attributes](./wep-2026-03-01-cm-resource-canonical-attrs.md)
  — defines the resource surface and the BLOCKED protocol this WEP corrects for
  streams.
- [Generic `AsyncCall<T>` for CM async imports](./wep-2026-04-22-subtask-generic.md)
  — subtasks already use the asynchronous lowering and `wait_for_subtask`.
- [WASI HTTP Integration](./wep-2026-02-21-wasi-http.md) — the handler patterns
  that stand to gain concurrency.
