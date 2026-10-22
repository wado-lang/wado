# WEP: Concurrent Stream Copies

## Context

A Wado program cannot wait on two streams at once. `Stream::read` and
`StreamWritable::write` lower `stream.read` and `stream.write` with `async`
([Async Canonical Options](./wep-2026-07-25-async-stream-canonical.md)), but
each binding absorbs the BLOCKED answer and waits inside itself. The program
gets control back only once that one copy has settled.

`WaitableSet` waits on several things at once, but only on calls. A stream end
is not `Waitable`, since no event is left for the host to report once the
binding has waited (`waitable_set_rejects_stream.wado`).

`core:process` is where this stops being a missing nicety. A child writes to
stdout and stderr through pipes the kernel bounds at a few pages. A parent that
reads stdout to its end while the child fills stderr waits for good, and so does
the child. Reading both together is the only fix that keeps memory bounded, and
a parent feeding a child's stdin while reading its stdout has the same problem.

## Decision

A copy is started without waiting for it. The started copy is a value that
joins a `WaitableSet` like a call does, and finishing it hands back the stream
end with what the copy moved.

```wado
affine variant Pipe { Out(ReadCopy<u8>), Err(ReadCopy<u8>) }
impl Waitable for Pipe;

let mut set = WaitableSet::<Pipe>::new();
set.join(Pipe::Out(stdout.start_read(4096)));
set.join(Pipe::Err(stderr.start_read(4096)));
while !set.is_empty() {
    match set.wait().member {
        Out(copy) => {
            let [end, chunk] = copy.finish();
            out.extend(&chunk.items);
            if chunk.result == CopyResult::Completed {
                set.join(Pipe::Out(end.start_read(4096)));
            }
        },
        Err(copy) => { ... },
    }
}
```

Everything here is the Component Model's own: `stream.read` and `stream.write`
with `async`, `waitable.join`, `waitable-set.wait`, and `stream.cancel-read` and
`stream.cancel-write`. No host extension is involved, and no feature gate past
the one Wado's streams already require.

### The surface

- `Stream<T>::start_read(self, max: i32) -> ReadCopy<T>` starts one copy of up
  to `max` elements.
- `ReadCopy<T>::finish(self) -> [Stream<T>, StreamChunk<T>]` returns the end
  and what the copy moved. It waits for the copy where the copy has not
  settled, so a program that never joins a set still reads correctly.
- `StreamWritable<T>::start_write(self, data: List<T>) -> WriteCopy<T>` and
  `WriteCopy<T>::finish(self) -> [StreamWritable<T>, StreamWrite<T>]` are the
  writing side.
- Both started copies implement `Waitable`, so a variant holding them derives
  it with `impl Waitable for V;`.
- `ReadyEvent` gains `Copied(CopyResult)`, the event the host reported for a
  copy.
- Every element type takes them. The compiler synthesizes the start and the
  finish per element type, as it synthesizes the copy today.

### Handlers

A handler for `Stream<T>` or `StreamWritable<T>` is how a test feeds a program
input or captures its output, so the started copy has to go through it. The
copy starts with a resource operation, `Stream::begin_read(&self, max)` or
`StreamWritable::begin_write(&self, data)`, which returns a `StartedRead<T>` or
`StartedWrite<T>`. `start_read` and `start_write` call it and take the end, and
so do the loops. A handler answers with a copy that has already settled,
`StartedRead::settled(chunk)` or `StartedWrite::settled(written)`: its data is
in memory, so it has no host copy to wait on. Such a copy joins a set as ready,
and dropping it cancels nothing.

The operations are public, since a handler in another package implements them.
A program that calls `begin_read` itself holds no copy in its end, so a second
copy on that end traps where the ownership would have refused it.

### One way to copy

`Stream::read` and `StreamWritable::write` are removed. Each was a started copy
finished at once, and keeping both would leave two implementations of one copy.
The loops stay, rebuilt on the started copy: `read_to_end`,
`read_to_end_sized`, `read_to_end_bytes`, `write_all` and `write_raw_all`. They
borrow the end as before, so a program that reads a stream to its end reads it
as it did.

### Ownership

Starting a copy moves the end into it. The Component Model traps on a second
copy on an end that is copying, and on a synchronous copy against an end in a
waitable set. Holding the end inside the started copy makes both unwritable.

### Settling

The host reports a copy's event once. A set that reports it records the packed
result by handle, as it records a delivered subtask resolution, and `finish`
reads it there. A copy that did not block at the start holds its result and has
no handle to report, so it joins a set as ready without the host.

### Dropping

Dropping a started copy that has not settled cancels it, since the end and the
buffer the host writes into cannot be released under a copy in flight. The
cancel leaves the set first: the Component Model traps on a synchronous cancel
against an end in a waitable set. A copy whose event a set already delivered is
not cancelled, since the Component Model traps on cancelling an end that is no
longer copying. Either way the end drops with it.

This makes the cancels reachable for the first time, from the drop alone. A
program holding an end holds no copy in flight, so it never cancels one itself.

## Roadmap

1. The started copy for every element type: `start_read`, `start_write`,
   `Copied`, the delivered copy table, cancel on drop and the handler
   operations, with e2e fixtures for a read that blocks, two reads settled out
   of order, a dropped copy in flight, a record element type, and a handler
   answering a started copy.
2. `read` and `write` removed, the loops rebuilt on the started copy, and every
   caller in the repository moved to a loop or to the started copy.
3. `waitable_set_rejects_stream.wado` kept, since a bare end stays not
   `Waitable`, beside a fixture that waits on a started copy.

[`core:process`](./wep-2026-10-21-core-process.md) builds on this: its `output`
and `read` read stdout and stderr through one set.

## Known gaps

- A future's read cannot be started. A future is read once, so nothing has
  needed to wait on one beside a stream yet.
