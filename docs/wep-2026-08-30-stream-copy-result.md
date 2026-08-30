# WEP: Stream Copy Results

A CM copy reports how it ended. `Stream::read` throws that report away, so the
end of a stream is unobservable and the next read traps. This WEP gives the copy
result a Wado type and hands it back to the caller.

## Context

`stream.read` and `stream.write` return a packed `(count << 4) | status`, where
the status is a `CopyResult` (`CanonicalABI.md`):

```python
class CopyResult(IntEnum):
  COMPLETED = 0   # this copy finished; says nothing about the peer's future
  DROPPED   = 1   # the peer end is gone; no further copy on this end
  CANCELLED = 2   # a cancel finished and returned the buffer; the end stays usable
```

Wado's adapters keep the count and discard the status
(`rt.wado::cm_stream_read_u8`, `resource_rewrite::synthesize_stream_read_func`),
so `Stream::read` returns a bare `List<T>` and end-of-stream is inferred from an
empty list.

### The inference is wrong

A copy can report elements _and_ `DROPPED` together — a peer that writes its last
chunk and drops produces exactly that. The reader gets a non-empty list, loops,
reads again, and the canonical traps:

```
cannot read after being notified that the writable end dropped
```

The trap is not a race. `done` is set only when this end itself receives
`DROPPED` (`futures_and_streams.rs:3956` on the direct return,
`:4879` on the event that settles a blocked copy). A peer that dropped without us
noticing yields `Dropped(0)` on the next read (`:3946`), never a trap. The state
is local, monotone, and caused by our own observation — which is why recording it
where the caller can see it is a complete answer.

Nothing about this is specific to a guest peer. `Dropped(n)` carries a count, so
a host that writes and drops in one go produces the same shape; today's WASI
paths stay green only because their writes and drops arrive separately.

### Writes have the same shape

`stream.write` reports `(count, status)` too: a reader may take a prefix, and the
peer may have dropped. `StreamWritable::write` returns `()` and loops internally
until the buffer drains or the status turns non-zero
(`rt.wado::cm_stream_write_drain`), so the caller learns neither how much was
taken nor that the reader is gone — and its next `write` traps
(`futures_and_streams.rs:3507`).

### Futures already do it

`Future::read` consults the status and maps a non-`COMPLETED` copy to
`Option::None` (`resource_rewrite.rs:1054`). Streams are the odd one out.

## Decision

### The copy result is a type

```wado
/// How one CM copy ended (`CanonicalABI.md`, `CopyResult`).
pub enum CopyResult {
    /// The copy finished. It says nothing about the peer's future.
    Completed,
    /// The peer end is gone; no further copy is possible on this end.
    Dropped,
    /// A cancel finished and returned the buffer; the end stays usable.
    Cancelled,
}
```

A `bool` would collapse `Dropped` and `Cancelled`, which the CM keeps apart: only
the first ends the stream. It would also invite reading the flag as the stream's
state, when what it describes is the copy that just happened.

### A read and a write return what the canonical reported

```wado
pub struct StreamChunk<T> {
    pub items: List<T>,
    pub result: CopyResult,
}

pub struct StreamWrite {
    pub count: i32,
    pub result: CopyResult,
}

impl Stream<T> {
    fn read(&self, max: i32) -> StreamChunk<T>;
}

impl StreamWritable<T> {
    fn write(&self, data: List<T>) -> StreamWrite;
}
```

Each is one canonical call. `write` no longer drains: a partial copy is what the
canonical reported, and the loop that finishes the buffer moves to the wrapper
below, out of the compiler and into Wado.

The primitives stay faithful to the CM, and the ergonomic layer sits on top of
them — the same split the language applies elsewhere. A caller that honours the
result it was handed can no longer reach the trap.

### The stdlib carries the loops

```wado
impl Stream<T> {
    /// Read until the writable end drops.
    pub fn read_to_end(&self) -> List<T>;
}

impl StreamWritable<T> {
    /// Write every element, or stop early if the readable end drops.
    pub fn write_all(&self, data: List<T>) -> CopyResult;

    /// The same loop over `write_raw`, for elements already in one array.
    pub fn write_raw_all(&self, data: Slice<T>) -> CopyResult;
}
```

These are ordinary Wado, generic over the element type, and are what most call
sites use. A call site that streams — bounded memory, incremental work — writes
the loop over `read` itself.

### A generic resource dispatches like any other declaration

`impl Stream<T>` is the first inherent impl on a generic resource, and the
receiver-naming path did not know the shape: `base_type_name`,
`impl_receiver_key`, `fq_base_type_name`, `generic_dispatch_components` and
`get_struct_name_from_type` each enumerated `GenericInstance` and stopped there.
An impl header and the calls to it then mangled under two different heads, so
the methods and their call sites never met and monomorphize minted no instance.
A generic resource is declared like the rest, and each of those answers now says
so.

### CM binding synthesis moves after monomorphization

A generic `impl Stream<T>` body cannot hold a `#[cm]` call today: binding
synthesis runs before monomorphize (`synthesis.rs`, `lib.rs`), so the payload is
still a type parameter and classification fails with

```
`T` has no Component Model representation as a `stream` element
```

`read_to_end` and `write_all` are exactly such bodies, so the payload-driven half
of synthesis moves after monomorphize, where every element type is concrete.
`reflect_bridge` is the precedent: a synthesis pass over `FlatPackage` that runs
there for the same reason.

The generated bodies call generic stdlib functions (`List<Elem>::with_capacity`),
whose instantiations monomorphize would already have had to mint. Synthesis
therefore runs to a fixpoint with monomorphize rather than strictly after it.

## Consequences

Positive:

- The end of a stream is a value the caller receives, not a shape it guesses.
  The read-after-dropped trap becomes unreachable for code that reads its result.
- A partial write is visible, and a reader that went away is reported rather than
  silently ending the transfer.
- `stream.write`'s drain loop leaves the compiler for the stdlib, where it is
  readable Wado rather than synthesized TIR.
- A generic wrapper over a CM primitive becomes expressible at all, which the
  pipeline order previously forbade.

Negative:

- Every stream call site changes shape, the stdlib and the packages included.
- Moving synthesis past monomorphize reorders a large phase (`cm_binding` is
  ~13.7k lines) and makes its interaction with monomorphize a fixpoint rather
  than a sequence.

Neutral:

- `Future::read` keeps its `Option<T>`: a future carries one value, and the
  status it already consults is what makes the option `None`.

## Known gaps

- Operating on an end whose peer has dropped still traps rather than being
  defined. Closing it means the end value carrying its own copy status — the
  `AsyncCall<T>` shape, a struct pairing the raw handle with what the CM told us
  — so that a post-`Dropped` read answers an empty chunk instead of reaching the
  canonical. `Stream<T>` is an opaque `i32` handle today
  (`GenericResource` → `WirType::I32`) with nowhere to keep it, and the
  `&mut self` route is blocked by D6 below.
- D6 of [Reference Representation](./wep-2026-06-13-reference-representation.md):
  `&mut resource` has no stable cell, so a method cannot write a handle back to
  the caller's binding. Independent of this WEP, and a prerequisite for any
  design that mutates an end in place.
- `BLOCKED` stays absorbed inside the adapters and `Stream` is still not joinable
  to a `WaitableSet`, so `select` over several streams and `cancel_read` /
  `cancel_write` remain unreachable — see
  [Async Canonical Options](./wep-2026-07-25-async-stream-canonical.md).

## Related WEPs

- [Async Canonical Options for `stream.read` / `stream.write`](./wep-2026-07-25-async-stream-canonical.md)
  — the BLOCKED protocol these copies run under.
- [Redesign Wasm CM Builtins as Resource Canonical Attributes](./wep-2026-03-01-cm-resource-canonical-attrs.md)
  — the resource surface the stream methods are declared on.
- [TIR-Level CM Binding Synthesis](./wep-2026-02-15-cm-binding-synthesis.md)
  — the phase this WEP moves.
- [Wasm CM Component Import](./wep-2026-06-26-wasm-cm-component-import.md)
  — component-to-component streaming, which made the discarded status
  deterministic rather than latent.
