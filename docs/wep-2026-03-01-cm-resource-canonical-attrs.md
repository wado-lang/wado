# WEP: Redesign Wasm CM Builtins as Resource Canonical Attributes

## Context

Component Model (CM) async primitives — `stream`, `future`, `waitable-set`, `subtask`,
`error-context`, and `task.return` — are currently declared as bare functions in
`builtin.wado` with `#[canonical("wasi", "...")]` attributes. The connection between
these low-level builtins and the user-facing resource types (`Stream<T>`, `Future<T>`, etc.)
is entirely hard-coded in Rust across five compiler phases:

```
method_call.rs   →  type name matching   →  synthetic "builtin::stream_create_pair" call
translate.rs     →  try_translate_*       →  WIR inline synthesis (100+ lines each)
dce.rs           →  ("Stream", "close")   →  canonical import dependency injection
wasm_plan.rs     →  TirImport filtering   →  ComponentPlan.canonical_intrinsics
component.rs     →  canonical name match  →  CM builder instruction
```

Additionally, `stream.wado` provides thin wrappers (`stream_new()`, `waitable_set_new()`, etc.)
around `builtin::*` functions, adding an unnecessary indirection layer.

### Problems

1. **Scattered knowledge** — CM resource knowledge is hard-coded across 5 Rust files
   with no single source of truth connecting resource methods to canonical operations
2. **builtin.wado pollution** — 13 CM functions mixed with ~60 Wasm instruction builtins
3. **Invisible contract** — resource declarations in `types.wado` have no bodies and
   nothing links them to canonical operations except hard-coded Rust
4. **Raw i32 handles** — `builtin::stream_read(rx, ptr, len)` bypasses the type system;
   `WaitableSet` and `Subtask` exist only as raw i32 values
5. **Missing operations** — `Future::read`, `WaitableSet`, `Subtask`, `ErrorContext`
   are not user-facing types

## Decision

Redesign the CM resource architecture from first principles. Resource method declarations
in `types.wado` become the single source of truth via `#[canonical]` attributes. The compiler
pipeline is restructured so that canonical intrinsic discovery flows from these attributes
through the compilation phases, replacing all hard-coded type/method name matching.

### Resource Declarations (types.wado)

```wado
pub resource Stream<T> {
    #[canonical("stream-new")]
    fn new() -> [Stream<T>, StreamWritable<T>];

    #[canonical("stream-read")]
    fn read(&self, max: i32) -> Array<T>;

    #[canonical("stream-close-readable")]
    fn close(&self);
}

pub resource StreamWritable<T> {
    #[canonical("stream-write")]
    fn write(&self, data: Array<T>);

    #[canonical("stream-close-writable")]
    fn close(&self);
}

pub resource Future<T> {
    #[canonical("future-new")]
    fn new() -> [Future<T>, FutureWritable<T>];

    #[canonical("future-read")]
    fn read(&self) -> T;

    #[canonical("future-close-readable")]
    fn close(&self);
}

pub resource FutureWritable<T> {
    #[canonical("future-write")]
    fn write(&self, value: T);

    #[canonical("future-close-writable")]
    fn close(&self);
}

/// Opaque token identifying a waitable handle within a WaitableSet.
/// Obtained from `Subtask::join` (and future `Stream::join`, etc.).
/// Compared by handle identity (==) to determine which waitable fired.
pub resource Waitable;

/// Result of `WaitableSet::wait` or `WaitableSet::poll`.
pub struct WaitEvent {
    pub code: i32,        // CM event code (stream-read=1, stream-write=2, etc.)
    pub handle: Waitable, // which waitable handle triggered the event
    pub payload: u32,     // event-specific data (e.g. count|status for streams)
}

pub resource WaitableSet {
    #[canonical("waitable-set-new")]
    fn new() -> WaitableSet;

    /// Block until an event occurs. Returns the event details.
    #[canonical("waitable-set-wait")]
    fn wait(&self) -> WaitEvent;

    /// Non-blocking poll. Returns Some(event) if an event is ready, None otherwise.
    #[canonical("waitable-set-poll")]
    fn poll(&self) -> Option<WaitEvent>;

    #[canonical("waitable-set-drop")]
    fn close(&self);
}

pub resource Subtask {
    #[canonical("subtask-drop")]
    fn close(&self);

    /// Join this subtask to a waitable set.
    /// Returns a Waitable token that identifies this subtask in wait results.
    #[canonical("waitable-join")]
    fn join(&self, set: &WaitableSet) -> Waitable;
}

pub resource ErrorContext {
    #[canonical("error-context-new")]
    fn new(message: String) -> ErrorContext;

    #[canonical("error-context-debug-message")]
    fn debug_message(&self) -> String;

    #[canonical("error-context-drop")]
    fn close(&self);
}
```

### Waitable: Typed Handle Token

`Waitable` is an opaque resource wrapping a CM handle (u32). It does not represent
a new CM concept — it is the same handle value that was joined, wrapped in a distinct
Wado type for type safety.

When `Subtask::join(set)` is called:
1. The CM `waitable-join(set_handle, subtask_handle)` is invoked
2. A `Waitable` wrapping `subtask_handle` is returned

When `WaitableSet::wait()` returns a `WaitEvent`:
1. The CM `waitable-set-wait(set, outptr)` writes `[handle, payload]` to linear memory
2. The adapter loads them and constructs `WaitEvent { code, handle: Waitable(handle), payload }`

The user compares `event.handle` against tokens from `join`:

```wado
let ws = WaitableSet::new();
let w1 = subtask1.join(&ws);
let w2 = subtask2.join(&ws);

let event = ws.wait();
if event.handle == w1 {
    // subtask1 fired
} else if event.handle == w2 {
    // subtask2 fired
}
```

`Waitable` auto-derives `Eq` (compares underlying handle values). It intentionally
does NOT expose the raw handle — users must go through `join` to obtain tokens.

### Naming: Future<T> as the Readable End

`Future<T>` is the readable end (CM `future-read` handle). The CM spec names these
`future-read` and `future-write`, so strictly speaking `Future<T>` should be
`FutureReadable<T>`. We keep `Future<T>` because:

- Most APIs receive futures (readable end); adding "Readable" is noise
- Matches JS/Python convention where "future"/"promise" means the consumer side
- `Stream<T>` has the same asymmetry (it's the readable end)

This is documented here for clarity. A future redesign could introduce `Future<T>` as
an effect or concept with `FutureReadable<T>` + `FutureWritable<T>` as the resource pair.

### WaitableSet Adapter Synthesis

`WaitableSet::wait` returns `WaitEvent` and `WaitableSet::poll` returns
`Option<WaitEvent>`. The CM adapter synthesis handles linear memory details:

For `wait`:
1. Allocate 8 bytes via `realloc`
2. Call `waitable-set-wait(set, addr)` → returns event code (i32)
3. Load handle and payload: `i32_load(addr)`, `i32_load(addr+4)`
4. Construct `WaitEvent { code, handle: Waitable(handle), payload }`

For `poll`:
1. Allocate 8 bytes via `realloc`
2. Call `waitable-set-poll(set, addr)` → returns event code or sentinel
3. If sentinel (no event): return `Option::None`
4. Otherwise: load, construct `Option::Some(WaitEvent { ... })`

For `Subtask::join`:
1. Call `waitable-join(set_handle, subtask_handle)` (void return in CM)
2. Return `Waitable(subtask_handle)` — wrap the joined handle as a token

### What Stays in builtin.wado

- All Wasm instruction builtins (`array_*`, `i32_load`, `f64_sqrt`, etc.)
- `realloc` (`#[canonical("mem", "realloc")]`) — linear memory management, not CM
- Bundled libm functions (`#[canonical("bundled", "...")]`)
- `call_indirect_stdout_write_via_stream` / `call_indirect_stderr_write_via_stream`
  — ambient I/O, handled by separate codegen path
- `inspect<T>` / `display<T>` — synthesis markers
- `task_return` (`#[canonical("wasi", "task-return")]`) — language statement, not a
  resource method

### What Gets Removed from builtin.wado

13 CM canonical functions:

- `stream_new`, `stream_read`, `stream_write`, `stream_drop_writable`, `stream_drop_readable`
- `future_new`, `future_write`, `future_drop_writable`, `future_drop_readable`
- `waitable_set_new`, `waitable_join`, `waitable_set_wait`
- `subtask_drop`

### What Gets Deleted

- `lib/core/stream.wado` — thin wrappers around `builtin::*`, replaced by resource methods

## Architecture

### Current Pipeline (hard-coded)

```
builtin.wado                     types.wado
  #[canonical("wasi", "...")]      resource Stream<T> { fn read(...); }
  fn stream_read(...)                  (no attributes, no bodies)
         │                                      │
         ▼                                      ▼
  BuiltinRegistry                   method_call.rs
  name → canonical_name              hard-coded: Stream → "stream_create_pair"
         │                                      │
         ▼                                      ▼
  dce.rs                            translate.rs
  hard-coded: ("Stream","read")       try_translate_stream_method(...)
  → add stream_read + waitable_*     try_translate_stream_writable_method(...)
  → populate TirImport list          try_translate_future_writable_method(...)
         │                                      │
         ▼                                      ▼
  wasm_plan.rs                      WirInstr::Call { func_id }
  filter namespace=="wasi"            via "builtin/stream_read" alias
  → ComponentPlan.canonical_intrinsics
         │
         ▼
  component.rs
  match "stream-read" → builder.stream_read(...)
```

### New Pipeline (attribute-driven)

```
types.wado (single source of truth)
  resource Stream<T> {
      #[canonical("stream-read")]
      fn read(&self, max: i32) -> Array<T>;
  }
         │
         ▼
  Resolver (method_lookup.rs)
  resource method with #[canonical] attr
  → MethodInfo { canonical_name: Some("stream-read") }
         │
         ▼
  WIR Translation (translate.rs)
  try_translate_canonical_method(method_info)
  match canonical_name:
    "stream-read" → emit_stream_read(...)
                    ctx.register_canonical("stream-read")
                    ctx.register_canonical("waitable-set-new")  // dependency
                    ctx.register_canonical("waitable-join")
                    ctx.register_canonical("waitable-set-wait")
         │
         ▼
  WirContext.needed_canonicals: IndexSet<CanonicalImport>
         │
         ▼
  ComponentPlan (wasm_plan.rs)
  reads from WirContext, not from TirImport
         │
         ▼
  Codegen (component.rs)
  match "stream-read" → builder.stream_read(...)
  (unchanged)
```

### Key Architectural Changes

#### 1. MethodInfo Carries Canonical Name

```rust
pub struct MethodInfo {
    // ... existing fields ...
    pub canonical_name: Option<String>,  // from #[canonical("...")] attribute
}
```

Populated by the resolver when resolving a resource method that has a `#[canonical]`
attribute. This is the bridge between resource declarations and WIR synthesis.

#### 2. Unified Canonical Method Dispatch

Replace three separate interceptors with a single dispatch point:

```rust
// Before: three separate functions, dispatched by receiver type
if let Some(instr) = self.try_translate_future_writable_method(...) { ... }
if let Some(instr) = self.try_translate_stream_method(...) { ... }
if let Some(instr) = self.try_translate_stream_writable_method(...) { ... }

// After: one function, dispatched by canonical attribute
if let Some(instr) = self.try_translate_canonical_method(method_info, ...) { ... }
```

The dispatch is on `canonical_name`, not on receiver type:

```rust
fn try_translate_canonical_method(&mut self, ...) -> Option<WirInstr> {
    let canonical = method_info.canonical_name.as_ref()?;
    match canonical.as_str() {
        "stream-new"             => self.emit_stream_new(...),
        "stream-read"            => self.emit_stream_read(...),
        "stream-write"           => self.emit_stream_write(...),
        "stream-close-readable"  => self.emit_drop_handle("stream-close-readable", ...),
        "stream-close-writable"  => self.emit_drop_handle("stream-close-writable", ...),
        "future-new"             => self.emit_future_new(...),
        "future-read"            => self.emit_future_read(...),
        "future-write"           => self.emit_future_write(...),
        "future-close-readable"  => self.emit_drop_handle("future-close-readable", ...),
        "future-close-writable"  => self.emit_drop_handle("future-close-writable", ...),
        "waitable-set-new"       => self.emit_waitable_set_new(...),
        "waitable-set-wait"      => self.emit_waitable_set_wait(...),
        "waitable-set-poll"      => self.emit_waitable_set_poll(...),
        "waitable-set-drop"      => self.emit_drop_handle("waitable-set-drop", ...),
        "waitable-join"          => self.emit_waitable_join(...),
        "subtask-drop"           => self.emit_drop_handle("subtask-drop", ...),
        "error-context-new"      => self.emit_error_context_new(...),
        "error-context-debug-message" => self.emit_error_context_debug_message(...),
        "error-context-drop"     => self.emit_drop_handle("error-context-drop", ...),
        _ => None,
    }
}
```

Each `emit_*` function registers its own canonical dependencies, so the knowledge of
"stream-read needs waitable-set-new + waitable-join + waitable-set-wait" lives in
`emit_stream_read`, not in DCE.

#### 3. Canonical Import Registration Moves to WIR

Currently canonical imports are collected by DCE (TIR phase) and stored in `TirImport`.
In the new design, they are registered during WIR translation:

```rust
// WirBuildContext
pub struct WirBuildContext {
    needed_canonicals: IndexMap<String, CanonicalImport>,
    // ...
}

pub struct CanonicalImport {
    pub canonical_name: String,   // "stream-read"
    pub params: Vec<WirType>,     // [I32, I32, I32]
    pub returns: Vec<WirType>,    // [I32]
}
```

Each synthesis function registers what it needs:

```rust
fn emit_stream_read(&mut self, ...) -> WirInstr {
    let stream_read = self.ctx.ensure_canonical("stream-read", &[I32, I32, I32], &[I32]);
    let ws_new = self.ctx.ensure_canonical("waitable-set-new", &[], &[I32]);
    let w_join = self.ctx.ensure_canonical("waitable-join", &[I32, I32], &[]);
    let ws_wait = self.ctx.ensure_canonical("waitable-set-wait", &[I32, I32], &[I32]);
    // ... emit WIR using these func IDs ...
}
```

`ensure_canonical` returns a `WirFuncId`, registering the import lazily if not
already registered.

#### 4. DCE Simplified

DCE no longer needs CM-specific knowledge. It performs standard call-graph reachability
analysis. The canonical import dependency injection (lines 202-252 in current `dce.rs`)
is removed entirely — that responsibility now belongs to the WIR synthesis functions.

DCE still handles:

- Removing unreachable functions
- Removing unused globals
- Standard dead code elimination

It does NOT handle:

- ~~Matching `("Stream", "close")` → add `stream_drop_readable`~~
- ~~Matching `("StreamWritable", "write")` → add `stream_write` + waitable_*~~
- ~~Populating `TirImport` for canonical intrinsics~~

#### 5. method_call.rs Simplified

The special cases for `Stream::new()` and `Future::new()` that synthesize
`"stream_create_pair"` / `"future_create_pair"` calls are removed. Instead,
the resolver finds the `new` method on the resource declaration, sees its
`#[canonical("stream-new")]` attribute, and passes it through as a normal
method call with `canonical_name` set in `MethodInfo`.

## Prelude Exports

`prelude.wado` adds the new types:

```wado
pub use {
    Option, Result,
    Stream, StreamWritable,
    Future, FutureWritable,
    Waitable, WaitEvent,
    WaitableSet, Subtask, ErrorContext,
} from "core:prelude/types.wado";
```

## E2E Tests

WaitableSet and Subtask are user-facing, so e2e tests are required:

```wado
// tests/fixtures/waitable_set_basic.wado
// Test that WaitableSet can be created and used directly
export fn run() {
    let ws = WaitableSet::new();
    ws.close();
    println("ok");
}

__DATA__
{"stdout": "ok\n"}
```

## Migration Path

1. Add `canonical_name` field to `MethodInfo`
2. Populate it from resource method `#[canonical]` attributes during resolution
3. Add `ensure_canonical` to `WirBuildContext` for lazy canonical import registration
4. Add `try_translate_canonical_method` to WIR translation, dispatch by canonical name
5. Move each existing `emit_*` function to register its own canonical dependencies
6. Route existing `try_translate_stream_method` etc. through the new unified dispatch
7. Remove CM-specific logic from DCE
8. Update `wasm_plan.rs` to collect canonicals from WIR context
9. Remove `method_call.rs` special cases for `Stream::new()` / `Future::new()`
10. Add `#[canonical]` attributes to resource methods in `types.wado`
11. Add `WaitableSet`, `Subtask`, `ErrorContext` resource declarations
12. Add `Future::read` declaration
13. Update `WaitableSet::wait`/`poll` to use Wado-level return types
14. Remove 13 CM functions from `builtin.wado`
15. Delete `stream.wado`
16. Update prelude exports
17. Add e2e tests for WaitableSet/Subtask
18. Implement `emit_future_read`, `emit_waitable_set_wait`, `emit_waitable_set_poll`,
    `emit_error_context_*` synthesis functions (or stub as compile errors)

## Consequences

**Positive:**

- Single source of truth: resource declarations in `types.wado` define both the
  user-facing API and the canonical operation mapping
- Adding a new resource method requires only: (a) declaration with `#[canonical]`
  in `types.wado`, (b) one `emit_*` synthesis function in `translate.rs`
- `builtin.wado` becomes clean: only Wasm instructions, libm, and `task_return`
- WaitableSet, Subtask, ErrorContext become first-class typed resources
- DCE is simpler and has no CM-specific knowledge
- Type safety: handles are typed resources, not raw i32; `Waitable` provides
  typed wait event identification without exposing raw handle values

**Negative:**

- The compiler still has a hard-coded `match` on canonical names in `translate.rs`
  (inherent — each operation needs different adapter logic)
- New synthesis functions needed for previously unimplemented operations
  (`future-read`, `waitable-set-wait` adapter, `waitable-set-poll`, `error-context-*`)

**Neutral:**

- No change to existing user-facing Stream/Future API
- No change to Wasm output for existing programs
- `task return` statement syntax unchanged
