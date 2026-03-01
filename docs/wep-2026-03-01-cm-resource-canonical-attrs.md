# WEP: CM Resource Canonical Attributes

## Context

Component Model (CM) async primitives — `stream`, `future`, `waitable-set`, `subtask`,
`error-context`, and `task.return` — are currently declared as bare functions in
`builtin.wado` with `#[canonical("wasi", "...")]` attributes. The connection between
these low-level builtins and the user-facing resource types (`Stream<T>`, `Future<T>`, etc.)
is entirely hard-coded in Rust across multiple compiler phases:

- `method_call.rs` matches type names to resolve `Stream::new()` → synthetic builtin call
- `wir_build/translate.rs` has `try_translate_stream_method`, `try_translate_stream_writable_method`,
  `try_translate_future_writable_method` — each 100+ lines of WIR generation
- `optimize/dce.rs` matches `("Stream", "close")`, `("StreamWritable", "write")` etc.
  to inject canonical builtin dependencies

This design has several problems:

1. **CM builtins pollute `builtin.wado`** — 14 CM functions mixed with ~60 Wasm instruction builtins
2. **Invisible contract** — the resource method declarations in `types.wado` have no bodies
   and nothing connects them to the canonical operations except hard-coded Rust
3. **Hard to extend** — adding a new resource method (e.g., `Stream::cancel_read()`)
   requires changes across 4+ Rust files
4. **No type safety** — `builtin::stream_read(rx, ptr, len)` takes raw `i32` handles;
   the type system doesn't enforce that `rx` is a Stream handle

## Decision

Move canonical operation declarations from `builtin.wado` to `#[canonical]` attributes
on resource method declarations in `prelude/types.wado`. The compiler's CM adapter
synthesis continues to generate the WIR glue code (memory allocation, BLOCKED handling,
GC↔linear memory conversion), but is driven by these attributes instead of hard-coded
type name matching.

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

    #[canonical("future-close-readable")]
    fn close(&self);
}

pub resource FutureWritable<T> {
    #[canonical("future-write")]
    fn write(&self, value: T);

    #[canonical("future-close-writable")]
    fn close(&self);
}

pub resource WaitableSet {
    #[canonical("waitable-set-new")]
    fn new() -> WaitableSet;

    #[canonical("waitable-set-wait")]
    fn wait(&self, out_addr: i32) -> i32;

    #[canonical("waitable-set-poll")]
    fn poll(&self, out_addr: i32) -> i32;

    #[canonical("waitable-set-drop")]
    fn close(&self);
}

pub resource Subtask {
    #[canonical("subtask-drop")]
    fn close(&self);

    #[canonical("waitable-join")]
    fn join(&self, set: &WaitableSet);
}
```

### What Stays in builtin.wado

- All Wasm instruction builtins (array_*, i32_load, f64_sqrt, etc.)
- `realloc` (`#[canonical("mem", "realloc")]`) — memory, not CM
- Bundled libm functions (`#[canonical("bundled", "...")]`)
- `call_indirect_stdout_write_via_stream` / `call_indirect_stderr_write_via_stream`
  — ambient I/O, handled by separate codegen path
- `inspect<T>` / `display<T>` — synthesis markers

### What Gets Removed from builtin.wado

All 14 CM canonical functions:
- `stream_new`, `stream_read`, `stream_write`, `stream_drop_writable`, `stream_drop_readable`
- `future_new`, `future_write`, `future_drop_writable`, `future_drop_readable`
- `task_return`
- `waitable_set_new`, `waitable_join`, `waitable_set_wait`
- `subtask_drop`

### What Gets Removed

- `stream.wado` — thin wrappers around `builtin::*` functions, no longer needed

### task.return

`task return expr;` is a language statement, not a resource method. Its canonical operation
`task-return` is emitted directly by the compiler when lowering the `task return` statement.
It does not belong to any resource type.

It will be declared as a standalone canonical function in `internal.wado`:

```wado
#[canonical("wasi", "task-return")]
fn task_return(result: i32);
```

This keeps it out of `builtin.wado` (it's CM, not a Wasm instruction) while remaining
accessible to the compiler's `task return` statement synthesis.

## Compiler Changes

### 1. Attribute Parsing (existing infrastructure)

The `#[canonical("...")]` attribute syntax is already parsed. Resource method declarations
are already function declarations in the AST. No parser changes needed.

### 2. Resource Method Resolution (method_call.rs)

Replace hard-coded type name matching with canonical attribute lookup:

```rust
// Before:
ResolvedType::Stream(inner) if method == "new" && args.is_empty() => {
    Some(("stream_create_pair", ...))
}

// After:
if let Some(canonical_name) = resource_method.canonical_attr() {
    // Dispatch to CM adapter synthesis based on canonical_name
}
```

### 3. WIR Translation (wir_build/translate.rs)

Replace `try_translate_stream_method` etc. with a unified `try_translate_canonical_method`:

```rust
fn try_translate_canonical_method(
    &mut self,
    receiver: &TirExpr,
    method_info: &MethodInfo,
    args: &[TirExpr],
    result_type: TypeId,
) -> Option<WirInstr> {
    let canonical_name = method_info.canonical_name.as_ref()?;
    match canonical_name.as_str() {
        "stream-read" => Some(self.emit_stream_read(...)),
        "stream-write" => Some(self.emit_stream_write(...)),
        "stream-close-readable" | "stream-close-writable" => Some(self.emit_close(...)),
        "stream-new" => Some(self.emit_stream_new(...)),
        "future-new" => Some(self.emit_future_new(...)),
        "future-write" => Some(self.emit_future_write(...)),
        "future-close-readable" | "future-close-writable" => Some(self.emit_close(...)),
        "waitable-set-new" => Some(self.emit_simple_canonical(...)),
        "waitable-set-wait" => Some(self.emit_simple_canonical(...)),
        "waitable-join" => Some(self.emit_simple_canonical(...)),
        "subtask-drop" => Some(self.emit_simple_canonical(...)),
        _ => None,
    }
}
```

The synthesis functions (`emit_stream_read`, `emit_stream_write`, etc.) remain unchanged
in their WIR output. Only the dispatch mechanism changes.

### 4. Canonical Intrinsic Registration

Currently, canonical intrinsics are discovered by scanning TIR imports for
`namespace == "wasi"`. With the new design, they are registered during WIR translation
when a canonical method is encountered.

The WIR build context will collect needed canonicals:

```rust
struct WirBuildContext {
    needed_canonicals: IndexSet<String>,  // "stream-new", "stream-read", etc.
    // ...
}
```

When `try_translate_canonical_method` emits adapter code, it registers the canonical:

```rust
self.ctx.needed_canonicals.insert("stream-read".to_string());
```

The component plan then uses this set instead of filtering TIR imports.

### 5. Dead Code Elimination (optimize/dce.rs)

Replace hard-coded `("Stream", "close")` matching with canonical attribute lookup
on the method info. The DCE phase already has access to method info — it just needs
to check for the canonical attribute instead of matching on type/method name strings.

### 6. MethodInfo Enhancement

Add an optional `canonical_name` field to `MethodInfo`:

```rust
pub struct MethodInfo {
    pub receiver_type: String,
    pub method_name: String,
    pub canonical_name: Option<String>,  // NEW: from #[canonical("...")] attribute
}
```

This field is populated during type resolution when a resource method with a
`#[canonical]` attribute is resolved.

## Migration Path

1. Add `canonical_name` field to `MethodInfo`
2. Populate it from resource method `#[canonical]` attributes during resolution
3. Add `try_translate_canonical_method` to WIR translation
4. Route existing synthesis code through the new dispatch
5. Update DCE to use canonical attributes
6. Update component plan to collect from WIR context
7. Remove CM functions from `builtin.wado`
8. Remove `stream.wado`
9. Add `WaitableSet` and `Subtask` resource types to `types.wado`
10. Move `task_return` to `internal.wado` with `#[canonical]`

## Consequences

**Positive:**
- CM operations are declared where they belong — on the resource types
- Adding a new resource method requires only: (a) declaration in `types.wado`,
  (b) one synthesis function in `translate.rs`
- `builtin.wado` becomes clean: only Wasm instructions and libm
- Resource method signatures document the user-facing API (typed handles, not raw i32)
- `WaitableSet` and `Subtask` become first-class types, enabling advanced async patterns

**Negative:**
- The compiler still has hard-coded synthesis for each canonical operation
  (this is inherent — each operation needs different adapter logic)
- The `#[canonical]` attribute on resource methods is a Wado-specific extension
  (but so was `#[canonical]` on builtins)

**Neutral:**
- No user-visible API change (Stream/Future methods remain the same)
- No Wasm output change (same canonical intrinsics, same adapter code)
- `task return` statement syntax unchanged
