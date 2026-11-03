# WEP: Ambient Logging Functions

## Context

### Problem Statement

Wado's effect system requires functions to explicitly declare their effects using `with` clauses. The `panic(String)` function was initially implemented as:

```wado
pub fn panic(message: String) -> ! with Stderr {
    eprintln(message);
    builtin::effect_wait();
    builtin::unreachable();
}
```

This creates a problem: any function using `panic()` or `assert` (which uses `panic()` internally) must declare `with Stderr`:

```wado
// Awkward: every function with assert needs Stderr
fn validate(x: i32) with Stderr {
    assert x > 0;
}
```

This is unergonomic because:

1. **Infectious effects**: `Stderr` propagates through the entire call graph
2. **Semantic mismatch**: `assert` is a debugging/correctness tool, not I/O
3. **Practical friction**: Users expect assertions without effect declarations

### Research: How Other Languages Handle This

| Language        | Approach                    | Notes                                  |
| --------------- | --------------------------- | -------------------------------------- |
| **Koka**        | `exn` effect for exceptions | Explicit, propagates through types     |
| **Rust**        | Untracked panic             | `panic!` has no effect in type system  |
| **Unison**      | `Exception` ability         | Explicit, but commonly used            |
| **WebAssembly** | `unreachable` is primitive  | Trap is not an effect, just terminates |

Many languages also provide "ambient" logging facilities that don't require explicit capability declarations:

- **Rust**: `log` crate with `error!`, `warn!`, `info!` macros
- **Python**: `logging` module
- **Java**: `java.util.logging`, SLF4J

These logging systems are "fire-and-forget" - they attempt to log but don't fail if logging is unavailable.

## Decision

### Introduce Ambient Logging Functions

Add two new functions that provide best-effort logging without effect requirements:

| Function         | Target | Effect Required | Behavior                                   |
| ---------------- | ------ | --------------- | ------------------------------------------ |
| `log(msg)`       | stdout | None            | Write if stdout available, no-op otherwise |
| `log_error(msg)` | stderr | None            | Write if stderr available, no-op otherwise |

These contrast with the strict I/O functions:

| Function        | Target | Effect Required | Behavior                   |
| --------------- | ------ | --------------- | -------------------------- |
| `println(msg)`  | stdout | `with Stdout`   | Guaranteed write to stdout |
| `eprintln(msg)` | stderr | `with Stderr`   | Guaranteed write to stderr |

### Implementation

```wado
/// Best-effort logging to stdout
/// Writes to stdout. Does not require effect declaration.
/// Waits for the async write to complete.
/// Future: will be no-op if stdout is not available in the world.
pub fn log(message: String) {
    let handles = builtin::stream_new();
    let rx = builtin::i64_low32(handles);
    let tx = builtin::i64_high32(handles);

    builtin::call_indirect_stdout_write_via_stream(rx);
    write_to_stream(tx, message, true);
    builtin::effect_wait();
}

/// Best-effort logging to stderr
/// Writes to stderr. Does not require effect declaration.
/// Waits for the async write to complete.
/// Future: will be no-op if stderr is not available in the world.
pub fn log_error(message: String) {
    let handles = builtin::stream_new();
    let rx = builtin::i64_low32(handles);
    let tx = builtin::i64_high32(handles);

    builtin::call_indirect_stderr_write_via_stream(rx);
    write_to_stream(tx, message, true);
    builtin::effect_wait();
}
```

The `builtin::call_indirect_*_write_via_stream` functions directly call the WASI write-via-stream functions without requiring effect declarations. This is implemented in the compiler's codegen.

### Updated `panic()` Implementation

```wado
/// Panic with a message
/// Logs the message to stderr (if available) and traps.
pub fn panic(message: String) -> ! {
    log_error(message);  // handles effect_wait() internally if stderr is available
    builtin::unreachable();
}
```

Note: `effect_wait()` is encapsulated inside `log_error()` rather than in `panic()`. This ensures:

- Wait only happens if we actually wrote to stderr
- No wasted wait when stderr is unavailable
- Cleaner `panic()` implementation

### Rationale

1. **Explicit special functions**: Instead of compiler magic for divergent functions, we have clearly named functions with documented "ambient" behavior.

2. **User choice**: Developers can choose between:
   - `println()`/`eprintln()` - strict, requires effect, guaranteed I/O
   - `log()`/`log_error()` - ambient, no effect, best-effort

3. **Pragmatic for debugging**: Logging and panic messages are debugging aids. Requiring effect declarations for them adds friction without meaningful safety benefits.

4. **Graceful degradation**: In worlds without stdout/stderr, logging silently becomes a no-op rather than failing compilation.

5. **Consistency**: Both stdout (`log`) and stderr (`log_error`) have ambient variants.

### `#[ambient(E, …)]` names its effects

The attribute first exempted a whole body from effect checking. Every function
that carried it performed a few effects it could name: `Stdout` or `Stderr`,
`Log` in the `core:log` facade, `KilnHost` in a Kiln generator's report. The
exemption hid every other effect too, so a facade that called `println` by
mistake drew no diagnostic. The attribute now lists its effects, and the rest of
the body is checked.

### `#[benign]` merges into `#[ambient]`, and the import always lands

Ambient effects were optional: where the target world lacked stdout or stderr,
`log_stdout` or `log_stderr` dropped its message, and the component imported no
stream. Extending that to every listed effect would have meant skipping any
ambient body, and returning a value its type supplies in its place. `#[benign]`
([WEP: Effect System and Randomness in Collections](./wep-2026-01-20-effect-system-randomness.md))
waived the same declaration but kept the import. The two differed only in
whether the import could be dropped, and dropping it buys nothing a host cannot
give more simply. A sandbox stays a sandbox when it links a stdout that writes
nowhere, and a Kiln generator's host routes its stdout and stderr to the build
log. Dropping the import cost machinery that decided it per world, and a write
that silently did nothing where its author expected it to run.

So one attribute remains, `#[ambient(E)]`, on a function or a global, and it
waives the declaration only. Each listed effect is imported wherever a call
reaches the body, and a host that does not offer it refuses the component. The
"unobservable" requirement `#[benign]` stated was never checked, and it goes
with the name: the attribute is the standard library's to write, which is
where its uses are audited.

A deterministic host refuses a clock rather than answering one. A Kiln
generator, or a program `core:eval` runs, that logs through
`TextSink<WallClock>` is refused, and the default sink, `TextSink<NoClock>`,
asks for none.

Hiding an effect from every caller is the standard library's business, for now:
a program writing `#[ambient]` is warned under `undeclared_effects`. The rule is
provisional, to be revisited as use cases appear.

## Consequences

### Positive

- `panic()` and `assert` work without effect declarations
- Clear API distinction: strict I/O vs ambient logging
- Users can choose the appropriate function for their needs
- No compiler magic or special cases for divergent functions
- Graceful behavior when I/O is unavailable

### Negative

- Two ways to write to stdout/stderr (potential confusion)
- Ambient functions bypass effect tracking (intentional trade-off)
- Silent no-op behavior may hide configuration issues

### API Summary

```
┌─────────────────────────────────────────────────────────┐
│                    Output Functions                      │
├─────────────────┬───────────────────────────────────────┤
│  Strict I/O     │  Ambient Logging                      │
│  (requires      │  (no effect needed,                   │
│   effect)       │   best-effort)                        │
├─────────────────┼───────────────────────────────────────┤
│  println(msg)   │  log(msg)         ← stdout            │
│  print(msg)     │                                       │
├─────────────────┼───────────────────────────────────────┤
│  eprintln(msg)  │  log_error(msg)   ← stderr            │
│  eprint(msg)    │                                       │
└─────────────────┴───────────────────────────────────────┘
```

## Implementation Notes

### Future: Assert Statement

The `assert` statement will use `panic()`:

```wado
// Compiler transforms:
assert condition, "message";

// Into:
if !condition {
    panic(`Assertion failed: message`);
}
```

Since `panic()` uses `log_error()`, no effect declaration is needed.

## Alternatives Considered

### Alternative 1: Divergent Function Exemption

Make functions returning `!` exempt from `Stderr` requirements.

**Rejected**: Compiler magic, harder to understand. The `log_error()` approach is more explicit.

### Alternative 2: Internal-Only Functions

Create `eprintln_without_effect()` as an internal function only for `panic()`.

**Rejected**: Less useful. Users benefit from ambient logging too.

### Alternative 3: Require Effects Everywhere

Keep strict effect requirements for all I/O, including panic.

**Rejected**: Too infectious, poor developer experience for debugging tools.

### Alternative 4: Global Debug Effect

Create an ambient `Debug` effect available everywhere.

**Rejected**: Adds complexity to the effect system. Simple functions are clearer.

## References

- [Koka Effect System](https://koka-lang.github.io/koka/doc/book.html)
- [Rust Panic Handling](https://doc.rust-lang.org/book/ch09-01-unrecoverable-errors-with-panic.html)
- [Rust log crate](https://docs.rs/log/latest/log/)
- [WebAssembly Trap Semantics](https://webassembly.github.io/spec/core/intro/overview.html)
