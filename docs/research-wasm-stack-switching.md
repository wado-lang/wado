# Wasm Stack Switching — Research Reference

This document describes the Wasm Stack Switching proposal, its relationship to Wado's
existing async model, and concrete use cases for the Wado language.

> **Source**: [WebAssembly Stack Switching Proposal](https://github.com/WebAssembly/stack-switching),
> `vendor/wasmtime/` (wasmtime implementation),
> `vendor/component-model/` (CM concurrency design).

---

## Table of Contents

1. [Stack Switching Primitives](#1-stack-switching-primitives)
2. [Current Async Architecture in Wado](#2-current-async-architecture-in-wado)
3. [Relationship to CM Async](#3-relationship-to-cm-async)
4. [Use Cases for Wado](#4-use-cases-for-wado)
5. [Non-Goals](#5-non-goals)

---

## 1. Stack Switching Primitives

The Wasm Stack Switching proposal (part of Wasm 3.0) introduces typed continuations
as first-class values. Core instructions:

| Instruction | Signature                               | Description                                               |
| ----------- | --------------------------------------- | --------------------------------------------------------- |
| `cont.new`  | `(ref $ft) -> (ref $ct)`                | Create a continuation from a function reference           |
| `resume`    | `(ref $ct, args...) -> results...`      | Resume a continuation with arguments                      |
| `suspend`   | `(tag, args...) -> results...`          | Suspend the current continuation, yielding to the resumer |
| `switch`    | `(ref $ct, tag, args...) -> results...` | Direct switch to another continuation (symmetric)         |
| `cont.bind` | `(ref $ct, args...) -> (ref $ct')`      | Partially apply arguments to a continuation               |

**Key properties:**

- **Typed**: Continuations carry type information; `suspend`/`resume` are type-safe.
- **One-shot by default**: Each continuation can be resumed at most once (linear resource).
- **Tag-based dispatch**: `suspend` specifies a tag; the resumer pattern-matches on tags
  to decide how to handle the suspension.

### Tags

Tags are declared at the module level and identify suspension points:

```wasm
(tag $yield (param i32))           ;; suspend with an i32 value
(tag $await (param (ref $future))) ;; suspend with a future handle
```

When a continuation calls `(suspend $yield 42)`, control returns to the resumer
with `$yield` as the event and `42` as the payload.

### Execution Model

```
Resumer                          Continuation
   |                                 |
   |-- resume(cont, args) --------->|
   |                                 | (runs user code)
   |<-- suspend($tag, values) ------|
   | (handle event)                  | (suspended)
   |-- resume(cont, results) ------>|
   |                                 | (continues)
   |<-- return(results) ------------|
   | (continuation finished)         |
```

---

## 2. Current Async Architecture in Wado

Wado achieves **colorless async** today without Wasm Stack Switching instructions.
The architecture relies on three layers:

### 2.1 CM Canonical Built-ins

WASI P3 defines async primitives as canonical ABI operations:

- `stream<T>`, `future<T>` — typed data channels
- `task.return` — deliver result without terminating the function
- `waitable-set.wait` — block until any waitable is ready
- `subtask` — handle for an in-flight async import call

These are the **guest-visible** async primitives. Wado's compiler synthesizes CM
binding adapters that use these operations (see `wado-compiler/src/synthesis/cm_binding.rs`).

### 2.2 Wasmtime's Stackful Async

Enabled by `config.wasm_component_model_async_stackful(true)` in `wado-cli/src/runtime.rs`.

When a guest calls `waitable-set.wait`, the host:

1. Suspends the guest's **host-managed fiber** (not a Wasm continuation)
2. Polls the underlying async runtime (tokio) for I/O completion
3. Resumes the fiber when any waitable becomes ready

This is an **opaque host mechanism** — the guest sees synchronous blocking semantics.
The Wasm module itself contains no continuation or fiber instructions.

### 2.3 Effect System

Wado's effect system (`with Stdout`, `with FileSystem`) tracks which functions
perform I/O. This is a **compile-time** mechanism orthogonal to the runtime async model.

### 2.4 What Already Works

```wado
// File I/O — completely synchronous-looking
export fn run() with Stdout, Preopens {
    let file = dir.open_at(flags, "hello.txt", oflags, dflags);
    let [stream, _done] = file.read_via_stream(0 as Filesize);
    let subtask = Stdout::write_via_stream(stream);
    wait_for_subtask(subtask);  // blocks via CM stackful async
}

// HTTP handler — async only at the export boundary
export async fn handle(request: Request) -> Result<Response, ErrorCode> {
    // ...
    task return Ok(response);    // delivers result via CM task.return
    trailers_tx.write(Ok(null)); // post-return execution
}
```

---

## 3. Relationship to CM Async

### CM Async Does Not Require Stack Switching

The CM concurrency model is designed to be **implementable without** Wasm Stack Switching.
Wasmtime implements it today using host-side fibers. The canonical operations
(`stream.read`, `future.write`, `waitable-set.wait`, etc.) are host-provided built-ins,
not Wasm instructions.

### CM Async Can Benefit From Stack Switching

When Wasm Stack Switching is available, the CM runtime can use native continuations
instead of host fibers. Benefits:

- **Portability**: No platform-specific fiber implementation needed
- **Performance**: Wasm-native stack switch is cheaper than host context switch
- **Composability**: Multiple Wasm components can switch stacks within a single
  host thread without fiber overhead

For Wado, this transition would be **transparent** — the same CM canonical operations,
same codegen, just faster runtime behavior.

### CM Thread Primitives

Wado already declares CM thread primitives in `wado-compiler/lib/core/builtin.wado`:

```wado
fn thread_yield(cancellable: i32);
fn thread_suspend(cancellable: i32) -> i32;
fn thread_suspend_to(target: i32, cancellable: i32) -> i32;
fn thread_suspend_to_suspended(target: i32, cancellable: i32);
fn thread_unsuspend(target: i32);
fn thread_new_indirect(func_type_index: i32, table_index: i32);
```

These are **CM-level** cooperative scheduling primitives, distinct from the Wasm Stack
Switching instructions. They operate on CM tasks/threads managed by the runtime.

---

## 4. Use Cases for Wado

### 4.1 Effect Handlers (Primary Use Case)

**Status in spec**: Designed but not implemented (spec.md § Handlers, marked TBD).

Effect handlers require the ability to **suspend a computation at an effect operation
and resume it** from the handler. This maps directly to stack switching:

- `suspend` at the effect operation site (e.g., `Generator::yield(value)`)
- `resume` from the handler after processing the effect

#### Generator Pattern (from spec.md)

```wado
interface Generator<T> {
    fn yield(value: T);
}

fn range(start: i32, end: i32) with Generator<i32> {
    let mut i = start;
    while i < end {
        Generator::yield(i);  // suspend here, pass i to handler
        i += 1;               // resume here when handler calls resume()
    }
}

fn collect_all() -> List<i32> {
    let mut result: List<i32> = [];

    with handler Generator<i32> {
        yield(value) => |resume| {
            result.push(value);
            resume();           // resume the continuation
        },
    } {
        range(0, 5);
    }

    return result;  // [0, 1, 2, 3, 4]
}
```

**Wasm compilation strategy:**

```wasm
;; Tag for the Generator::yield effect operation
(tag $generator_yield (param i32))

;; range() becomes a regular function; yield compiles to suspend
(func $range (param $start i32) (param $end i32)
  (local $i i32)
  (local.set $i (local.get $start))
  (block $break
    (loop $loop
      (br_if $break (i32.ge_s (local.get $i) (local.get $end)))
      (suspend $generator_yield (local.get $i))  ;; yield
      (local.set $i (i32.add (local.get $i) (i32.const 1)))
      (br $loop)
    )
  )
)

;; collect_all() creates a continuation and drives it
(func $collect_all (result (ref $array_i32))
  (local $cont (ref null $range_ct))
  ;; Create continuation from range function
  (local.set $cont (cont.new $range_ct (ref.func $range)))
  ;; Bind arguments: range(0, 5)
  (local.set $cont (cont.bind $range_ct $bound_ct (local.get $cont)
                      (i32.const 0) (i32.const 5)))
  ;; Resume loop
  (block $done
    (loop $drive
      ;; resume returns (tag, payload) when suspended, or finishes
      (block $on_yield (result i32)
        (resume $bound_ct (on $generator_yield $on_yield) (local.get $cont))
        (br $done)  ;; continuation finished normally
      )
      ;; Here: yielded value is on stack
      ;; ... append to result array ...
      (br $drive)  ;; resume again
    )
  )
)
```

#### Mock Handler (Testing)

```wado
handler MockStdout for Stdout {
    let mut output: List<u8> = [];

    write_via_stream(data) => {
        output.extend(collect_stream(data));
        return Ok(());
    },
}

test "captures output" {
    with Stdout => MockStdout {
        println("hello");  // intercepted by MockStdout handler
    }
    // assert MockStdout.output == ...
}
```

Without stack switching, the handler cannot intercept `println` mid-execution and
redirect I/O — the CM runtime owns the Stdout implementation. With stack switching:

1. `println` calls `Stdout::write_via_stream` → compiles to `(suspend $stdout_write ...)`
2. The handler intercepts the suspension, captures the data, and resumes
3. No actual WASI call is made — the handler short-circuits it

This enables **pure unit testing** of effectful code without WASI runtime involvement.

### 4.2 Structured Concurrency

**Status in spec**: Syntax shown in spec.md § Concurrency Model (`join`), not implemented.

```wado
fn load_data() -> Data with Http {
    let [users, posts] = join(
        || fetch_users(),
        || fetch_posts(),
    );
    return Data { users, posts };
}
```

**Two possible implementations:**

#### A. CM-Level (No Stack Switching Required)

For I/O-bound concurrent tasks that call WASI async imports:

1. Each closure calls an async import → gets a subtask handle
2. Both subtask handles are joined to a single waitable-set
3. `waitable-set.wait` blocks until one completes, then the other

This works today with existing CM primitives. `join()` can be implemented as a
library function using `waitable-set.wait` internally.

#### B. Guest-Level (Requires Stack Switching)

For CPU-bound concurrent tasks or mixed compute/I/O:

1. Each closure becomes a continuation via `cont.new`
2. A scheduler resumes each continuation in round-robin
3. Continuations `suspend` at yield points (explicit or compiler-inserted)

This requires stack switching because the guest needs to interleave execution
of multiple computations **within a single Wasm function call**.

**Recommendation**: Start with CM-level `join` for I/O concurrency (practical today),
defer guest-level scheduling to when stack switching is stable.

### 4.3 Lazy Iteration / Generators

Beyond effect handlers, stack switching enables **first-class generators** as
a language feature:

```wado
fn fibonacci() -> impl Iterator<i32> {
    let mut a = 0;
    let mut b = 1;
    loop {
        yield a;
        let next = a + b;
        a = b;
        b = next;
    }
}

for let n of fibonacci() {
    if n > 1000 { break; }
    println(`{n}`);
}
```

**Without stack switching**, Wado's iterators require manual state machine encoding
(implementing `Iterator` trait with `next()` method). This is verbose for complex
iteration patterns.

**With stack switching**, the compiler transforms `yield` into `suspend` and wraps
the generator in a continuation. The `for-of` loop drives the continuation with `resume`.

Note: This is a **special case of effect handlers** — `yield` is an effect operation
on a `Generator<T>` effect, and `for-of` is syntactic sugar for a handler that
collects values. The spec already shows this pattern (§ Continuation Control).

### 4.4 Coroutine-Based Stream Processing

Wado's current stream operations (`Stream<T>`) use CM canonical built-ins that go
through host memory. For **guest-internal** stream processing (no I/O), stack switching
enables zero-copy coroutine pipelines:

```wado
fn parse_lines(input: &String) with Generator<String> {
    let mut start = 0;
    for let mut i = 0; i < input.len(); i += 1 {
        if input.char_at(i) == '\n' {
            Generator::yield(input.slice(start, i));
            start = i + 1;
        }
    }
    if start < input.len() {
        Generator::yield(input.slice(start, input.len()));
    }
}

fn count_matching(input: &String, pattern: &String) -> i32 {
    let mut count = 0;
    with handler Generator<String> {
        yield(line) => |resume| {
            if line.contains(pattern) { count += 1; }
            resume();
        },
    } {
        parse_lines(input);
    }
    return count;
}
```

Each line is produced lazily via `suspend` and consumed immediately by the handler —
no intermediate `List<String>` allocation.

---

## 5. Non-Goals

### 5.1 Preemptive Multithreading

Stack switching provides **cooperative** concurrency only. True parallelism requires
`wasm-threads` (shared memory + atomics), which is a separate concern. Wado should
not conflate the two.

### 5.2 Replacing CM Async

Stack switching does **not replace** CM async primitives (`stream`, `future`,
`waitable-set`, `task.return`). These remain the correct mechanism for cross-component
and host I/O operations. Stack switching is for **guest-internal** control flow.

### 5.3 Async/Await Syntax

Even with stack switching, Wado should not introduce `async`/`await` keywords in
function implementations. The colorless model (effects + transparent suspension)
is a deliberate design choice. Stack switching **reinforces** this by making
suspension truly invisible at the language level.

---

## Appendix: Wasmtime Status

As of wasmtime v42 (2026-03):

| Feature              | Config Flag                                 | Status                          |
| -------------------- | ------------------------------------------- | ------------------------------- |
| CM Async             | `wasm_component_model_async(true)`          | Enabled                         |
| CM Async Built-ins   | `wasm_component_model_async_builtins(true)` | Enabled                         |
| CM Async Stackful    | `wasm_component_model_async_stackful(true)` | Enabled (host fibers)           |
| Wasm Stack Switching | `wasm_stack_switching(true)`                | Commented out (platform issues) |

The `wasm_stack_switching` flag is disabled in `wado-cli/src/runtime.rs:71` with
the comment "Not supported on macOS". When enabled, it would activate the
`cont.new`/`resume`/`suspend` instructions in cranelift's code generator.
