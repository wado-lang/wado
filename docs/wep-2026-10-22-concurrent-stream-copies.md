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

A copy can be started without waiting for it. The started copy is a value that
joins a `WaitableSet` like a call does, and finishing it hands back the stream
end with what the copy moved.

```wado
let mut set = WaitableSet::<Pipe>::new();
set.join(Pipe::Out(stdout.start_read(4096)));
set.join(Pipe::Err(stderr.start_read(4096)));
loop {
    let ready = set.wait();
    match ready.member {
        Out(read) => {
            let [end, chunk] = read.finish();
            out.extend(&chunk.items);
            if chunk.result == CopyResult::Completed { set.join(Pipe::Out(end.start_read(4096))); }
        },
        Err(read) => { ... },
    }
    if set.is_empty() { break; }
}
```

### The surface

- `Stream<u8>::start_read(self, max: i32) -> ReadCopy` starts one copy of up
  to `max` bytes.
- `ReadCopy::finish(self) -> [Stream<u8>, StreamChunk<u8>]` returns the end
  and what the copy moved. It waits for the copy where the copy has not
  settled, so a program that never joins a set still reads correctly.
- `StreamWritable<u8>::start_write(self, data: ByteList) -> WriteCopy`
  and `WriteCopy::finish(self) -> [StreamWritable<u8>, StreamWrite<u8>]`
  are the writing side.
- Both started copies implement `Waitable`, so a variant holding them derives
  it with `impl Waitable for V;`.
- `ReadyEvent` gains `Copied(CopyResult)`, the event the host reported for a
  copy.
- `Stream::read` and `StreamWritable::write` keep their signatures. They are a
  started copy finished at once.

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

This makes `Stream::cancel_read` and `StreamWritable::cancel_write` reachable
for the first time, from the drop alone. They stay uncallable from a program
holding the end, since it holds no copy in flight.

## Roadmap

1. `start_read` / `start_write` on byte streams, `Copied`, the delivered copy
   table, cancel on drop, and e2e fixtures for a read that blocks, two reads
   settled out of order, and a dropped copy in flight.
2. `core:process`'s `output` and `read` read stdout and stderr through one set.

## Known gaps

- Only byte streams start a copy. A stream of any other element type lifts its
  elements through bindings the compiler synthesizes per type, and those still
  wait inside.
- A future's read cannot be started either.
