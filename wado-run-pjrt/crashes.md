# Plugin Crashes

Every way a `wado-run-pjrt` process has died in native code, and what a plugin
was seen to refuse. A plugin runs inside this process, so its fault ends the
program with no Wasm trap and no message.

## Observed

### `libtpu.so` walks a wasmtime fiber's stack

- Status: fixed.
- Signal: `SIGSEGV`, intermittent. The same binary on the same input passed some
  runs and faulted others. It took a compile of Gemma 3; the tiny programs
  never reached it.
- Backtrace: `UnwindImpl`, `absl::GetStackTrace`,
  `absl::container_internal::SampleSlow`, then `raw_hash_set::insert` in
  `xla::ValidateDotDimensionNumbers`.
- Cause: abseil samples a hash table now and then, and records the stack by
  following frame pointers. A guest runs on a wasmtime fiber, so the walk left
  the fiber's frames and read an unmapped page.
- Guard: `plugin_thread`. Every call goes through `Plugin::enter`, on a thread
  whose stack is an ordinary one.
- Test: none is deterministic. `loams_programs_compile_for_a_tpu` covers the
  path, and an intermittent fault there means a call has left the thread.

### An upload shorter than its extents

- Status: fixed.
- Signal: `SIGSEGV` on 8 GiB of extents over 4 bytes. A smaller overrun would
  read host memory into the guest's tensor with no signal at all.
- Cause: `PJRT_Client_BufferFromHostBuffer` takes a pointer and extents, not a
  length. It read as far as a guest's extents said, past the guest's bytes.
- Guard: `Client::upload` checks that the bytes fill the extents, with overflow
  checked.
- Test: `a_plugin_refuses_what_a_guest_gets_wrong`.

### Empty compile options

- Status: fixed.
- Signal: `SIGABRT`.
- Cause: an empty `CompileOptionsProto` leaves the replica count at 0, and the
  CPU plugin aborts on it.
- Guard: `ONE_REPLICA_ONE_PARTITION`.
- Test: every test that compiles.

### A module nested past the plugin's stack

- Status: open.
- Signal: `SIGSEGV` on the stack's guard page, which ends as `SIGABRT`. On the
  CPU plugin Rust's handler reports
  `thread 'pjrt-plugin' has overflowed its stack`. On `libtpu.so` its own handler
  raises the signal.
- Backtrace, on the CPU plugin: `mlir::Lexer::lexToken`, then `parseBlock`, `parseRegion` and
  `parseGenericOperationAfterOpName` of MLIR's `OperationParser` in turn, to
  80,975 frames.
- Cause: MLIR's parser recurses per nested region, before anything of XLA runs.
  100,000 nested `stablehlo.if` overflow `STACK_BYTES`, on both plugins. 10,000
  come back as an error.
- Guard: none. The overflow ends the process without corrupting anything, and
  Loam's programs nest a region per `If`, far short of this.
- Upstream: LLVM 23.1.5's `mlir-opt` overflows the same way at 2,500 levels on
  an 8 MiB stack. llvm/llvm-project#63146 fixed only the verifier.

## Never observed

### The library unloading under the plugin's thread

- Status: fixed. A review found it by reading the code.
- Cause: `dlclose` at exit could race the plugin's thread and its thread-locals,
  which would fault after the program had succeeded and leave the exit status
  flaky.
- Guard: `Plugin::open` forgets the library, so it stays mapped for the process.

## What a plugin refuses

`a_plugin_refuses_what_a_guest_gets_wrong` holds the pinned CPU plugin to the
errors below, so a plugin upgrade cannot lose one silently. It runs under
`mise run test-pjrt`.

| A guest passes                                   | The plugin                  |
| ------------------------------------------------ | --------------------------- |
| Text that is not MLIR, or nothing                | Returns an error            |
| A module that does not verify, or has no `@main` | Returns an error            |
| The wrong number of arguments                    | Returns an error            |
| An argument of another byte size                 | Returns an error            |
| A donated buffer, to read or to execute          | Returns an error            |
| One buffer as two donated arguments              | Returns an error            |
| A topology it does not know                      | Returns an error            |
| An argument of the same byte size, any type      | Accepts it and reinterprets |
| An argument of the same byte size, any shape     | Accepts it and reinterprets |
| A `kept` index out of range, or negative         | Ignores it                  |

The last three stay in bounds, and no test holds them.

Nothing here has run on a TPU: with no device `libtpu.so` has no client, so
only its compile path and its refusal of an unknown topology were seen.

An output too large for memory returned an error at execute, but only under
`ulimit -v`. Without a limit it is untested, since it may reach the OOM killer.
