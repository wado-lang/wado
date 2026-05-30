# Component model: host stream/future transmits leak when the guest drops its end while the host consumer/producer is `HostReady`

## Summary

When a host registers a stream consumer via `StreamReader::pipe` (or hands the
guest a host-driven stream/future via `FutureReader::new` / a host-written
stream), and the **guest then drops its end of that stream/future**, the
`TransmitState` and both `TransmitHandle`s are **never reclaimed** from the
component's concurrent-state table. The host-side consumer/producer is left in
`HostReady` and is never finalized, so the slots leak for the lifetime of the
instance.

A guest that performs many such operations in a loop fills the concurrent-state
table and traps with `resource table has no free keys`.

This is directly observable through `wasi:cli/stdout.write-via-stream` /
`wasi:cli/stdin.read-via-stream`, but the root cause is in the core component
runtime (`futures_and_streams.rs`), not in `wasmtime-wasi` — any host that uses
the public `StreamReader::pipe` / `FutureReader` APIs is affected.

## Affected (current `main`)

`crates/wasmtime/src/runtime/component/concurrent/futures_and_streams.rs`

Two match arms are no-ops where they must finalize the host end:

* `Instance::host_drop_writer` — when the guest drops the **writable** end:

  ```rust
  ReadState::HostReady { .. } | ReadState::HostToHost { .. } => {}
  ```

  A host consumer registered via `pipe`/`set_consumer` is only driven
  reactively on guest writes. When the guest drops the writer it is never
  re-polled, never observes the close, and is never dropped. (For
  `wasi:cli/stdout`, `OutputStreamConsumer` only sends its `result_tx` on an
  I/O error — on a clean close it relies on being dropped, which never
  happens — so the dependent `write-via-stream` future also never resolves.)

* `Instance::host_drop_reader` — when the guest drops the **readable** end:

  ```rust
  WriteState::HostReady { .. } => {}
  ```

  A host producer (`FutureReader::new`'s producer, or a host-written stream)
  in `WriteState::HostReady` is never finalized when the guest drops the
  reader, so its transmit leaks.

A `TransmitState` is only deleted in `delete_transmit` once **both** ends reach
`Dropped`; with these no-ops one end stays `HostReady` forever.

## Reproductions

Two minimal components (attached). Each leaves **6** entries in the
concurrent-state table after `run()` returns (1 stream transmit = state + 2
handles, 1 future transmit = state + 2 handles):

### 1. `leak_write_via_stream.wat` — host *consumer* (stream) + host *producer* (future)

Guest: `stream.new`, hand the readable end to `wasi:cli/stdout.write-via-stream`
(host registers a consumer via `pipe`), write to the writable end, drop it,
then drop the readable end of the returned future without reading it.

* Stream → `host_drop_writer` with `ReadState::HostReady` (host consumer) leaks.
* Future → `host_drop_reader` with `WriteState::HostReady` (host producer) leaks.

### 2. `leak_read_via_stream.wat` — host *producer* (stream + future)

Guest: call `wasi:cli/stdin.read-via-stream()` (host produces both the stream
and the future), then drop both readable ends without reading.

* Stream + Future → `host_drop_reader` with `WriteState::HostReady` leaks.

### Harness (sketch)

```rust
// async + component-model-async enabled engine; wasmtime-wasi p3 cli linked.
let component = Component::new(&engine, include_str!("leak_write_via_stream.wat"))?;
let instance = linker.instantiate_async(&mut store, &component).await?;
let run = instance.get_typed_func::<(), (Result<(), ()>,)>(&mut store, "run")?;
run.call_async(&mut store, ()).await?;
store.assert_concurrent_state_empty(); // panics: non-empty table: [3, 4, 5, 7, 8, 9]
// equivalently: assert_eq!(store.concurrent_state_table_size(), 0); // == 6
```

## Proposed fix

Finalize the stranded host end on guest end-drop:

* In `host_drop_writer`, when the read side is `HostReady`/`HostToHost` and the
  writer is now `Dropped`, set read to `Dropped` and `delete_transmit` (which
  drops the consumer, releasing e.g. `OutputStreamConsumer::result_tx` so the
  dependent future resolves).
* In `host_drop_reader`, when the write side is `HostReady`, finalize the host
  producer and `delete_transmit`.

A local patch covering exactly these two arms takes both reproductions from 6
leaked entries to 0 with no test regressions. (Happy to open a PR.)

## Note on test coverage

The async leak tests that call `assert_concurrent_state_empty`
(`round_trip*`, `post_return`) use guest↔guest / read-based flows. The
`.pipe()`-based host-consumer scenarios (`streams.rs`, `transmit.rs`) do **not**
assert an empty concurrent state, so this drop path appears to be untested.
