# ADR: Closure Implementation

**Date**: 2026-01-16
**Status**: Proposed

## Context

Wado supports closure syntax (parsed but not codegen yet):

```wado
let f = |x| { return x + 1; };
let g = || { return count; };  // Captures outer variable 'count'
```

The language design requires:

1. First-class functions (closures can be passed as values)
2. Capture by reference (per [ADR: Value Semantics and Reference Captures](./adr-2026-01-12-value-semantics-and-captures.md))
3. Multiple closures can capture the same variable
4. Closures must work with effect system and `captures[...]` tracking
5. Efficient representation targeting Wasm GC

### Current TIR Representation

The resolver already produces a `TirExpr::Closure` node:

```rust
Closure {
    params: Vec<(String, TypeId)>,
    body: Box<TirExpr>,
    captures: Vec<TirCapture>,  // Analyzed at resolve time
}

struct TirCapture {
    name: String,
    outer_index: u32,   // Index in outer function's locals
    type_id: TypeId,
    is_mut: bool,
}
```

The analyzer identifies which outer variables are captured and tracks them in the `captures` field.

### WebAssembly Closure Implementation Options

#### Option 1: Closure Struct + Funcref (Wasm GC Native)

Create a Wasm GC struct to hold the environment, paired with a function reference.

**Representation:**

```wat
;; Environment struct (one per closure type)
(type $ClosureEnv_0 (struct
  (field $count (mut i32))    ;; Captured mutable variable
  (field $x (ref any))        ;; Captured struct
))

;; Function taking environment as first parameter
(type $ClosureFn_0 (func (param (ref $ClosureEnv_0)) (result i32)))

;; Closure value = struct with env + funcref
(type $Closure_0 (struct
  (field $env (ref $ClosureEnv_0))
  (field $func (ref $ClosureFn_0))
))
```

**Calling a closure:**

```wat
;; Call: f()
(local.get $f)                        ;; Get closure struct
(struct.get $Closure_0 $env)          ;; Extract environment
(local.get $f)
(struct.get $Closure_0 $func)         ;; Extract function
(call_ref $ClosureFn_0)               ;; Call with env as first param
```

**Pros:**

- Native Wasm GC representation
- Environment is garbage collected automatically
- Multiple closures can share the same environment struct (reference semantics)
- Efficient: no copying, direct field access
- Type-safe: each closure has distinct struct type

**Cons:**

- Requires generating unique struct types for each closure signature
- More complex codegen: need to generate struct types, allocation, field access
- Not directly compatible with Component Model funcref (needs adapter)

#### Option 2: Flat Closure (Trampoline-based)

Convert closures to plain functions by passing captured variables as additional parameters. Use trampolines to adapt call sites.

**Representation:**

```wat
;; Original closure: |x| { return x + count; }
;; Generated function with captures as params:
(func $closure_impl (param $x i32) (param $count_capture i32) (result i32)
  (i32.add (local.get $x) (local.get $count_capture))
)

;; Trampoline created at closure creation site:
;; let f = |x| { return x + count; };
(func $trampoline_0 (param $x i32) (result i32)
  (call $closure_impl
    (local.get $x)
    (local.get $count)  ;; Capture bound here
  )
)
```

**Closure value:** Just a function reference (funcref)

**Pros:**

- Simplest representation: closure = funcref
- Compatible with Component Model function types directly
- No need for environment struct types
- Good for closures with few captures

**Cons:**

- **Cannot share mutable state**: Each trampoline captures by value, not reference
  - Violates the design requirement that closures capture by reference
  - Multiple closures capturing the same variable get independent copies
- Trampolines increase code size (one per closure creation site)
- Doesn't support mutable captures correctly (mutations don't propagate)

**Example of the problem:**

```wado
let mut count = 0;
let inc = || { count += 1; };
let get = || { return count; };

inc();
inc();
println(get());  // Should print 2, but trampoline approach would print 0
```

With trampolines, `inc` and `get` each get their own copy of `count`—mutations don't propagate.

#### Option 3: Defunctionalization

Convert closures to an enum of closure types and an interpreter function.

**Representation:**

```wat
;; Closure types as enum variants
(type $ClosureKind (enum
  (case $Inc)
  (case $Get)
  (case $Adder (i32))
))

;; Interpreter
(func $call_closure (param $kind (ref $ClosureKind)) (param $arg i32) (result i32)
  (match (local.get $kind)
    (case $Inc ...)
    (case $Get ...)
    (case $Adder ...)
  )
)
```

**Pros:**

- Simple to implement
- Works without function references

**Cons:**

- **Very inefficient** for Wasm: every closure call becomes indirect
- Large interpreter function grows with each closure type
- Not idiomatic for Wasm GC (which has native funcref)
- Poor performance: match + dispatch overhead

#### Option 4: Component Model Resource-based Closures

Use Component Model resources to represent closures with explicit lifetime management.

**Representation:**

```wit
// In WIT
resource closure-env {
    call: func(arg: s32) -> s32;
}
```

```wat
;; In core Wasm
(type $env (struct (field $count (mut i32))))

;; Closure represented as resource handle (i32)
;; Mapping from handle -> (env, func)
```

**Pros:**

- Clean Component Model boundary
- Explicit ownership semantics

**Cons:**

- Resources require explicit drop (no automatic GC)
- Handle indirection overhead
- More complex than native GC approach
- Awkward for internal (non-exported) closures
- Requires a runtime table to map handles to (env, func) pairs

### Closure Types and Type System Integration

Closures have function types with capture annotations:

```wado
// Type: Fn(i32) -> i32 with captures[count]
let f = |x| { return x + count; };

// Generic function taking a closure
fn apply<T, R>(f: Fn(T) -> R, x: T) -> R {
    return f(x);
}
```

**Type representation challenge:**

- In AST/TIR: `FunctionType` with `captures: Vec<String>`
- In Wasm: Needs to map to `funcref` type

**Subtyping:**

```wado
Fn(T) -> R                    // Pure function (no captures)
  <: Fn(T) -> R with captures[x]  // Can capture x
```

A pure function can be used where a capturing function is expected, but not vice versa.

### Comparison with Other Languages

| Language     | Approach                                               | Wasm Target           |
| ------------ | ------------------------------------------------------ | --------------------- |
| **Rust**     | Closure traits (Fn, FnMut, FnOnce) + struct for env   | Struct + funcref      |
| **Go**       | Closure struct with env pointer                        | Struct + funcref      |
| **Swift**    | Closure = function + captured vars in struct           | Struct + funcref      |
| **OCaml**    | Closure = code pointer + environment block             | Similar to struct     |
| **Scheme**   | First-class continuations, heap-allocated environments | GC struct + funcref   |
| **AssemblyScript** | Closure = funcref + trampolines                    | Trampolines (no GC)   |

Most languages with GC targeting Wasm use **Option 1** (struct + funcref) because it provides:

- Shared mutable state
- Efficient representation
- Native GC support

### Integration with `captures[...]` Tracking

Per the [Value Semantics ADR](./adr-2026-01-12-value-semantics-and-captures.md), closures that capture variables require `captures[...]` annotation in function types:

```wado
// Closure type with captures
fn register(f: Fn(i32) -> i32 with captures[0]) {
    callbacks.push(f);
}

// Inferred captures
let count = 0;
let f = |x| { return x + count; };
// Type: Fn(i32) -> i32 with captures[count]
```

**Implementation requirement:**

- When a closure is created, check if it captures any variables
- If yes, mark the closure type with `captures[...]` listing captured variables
- At call sites passing closures to functions, verify `captures[...]` compatibility
- Heap-promote captured variables if they escape through the closure

**Heap promotion:**

```wado
fn make_counter() -> Fn() -> i32 with captures[count] {
    let mut count = 0;  // Must be heap-promoted
    return || {
        count += 1;
        return count;
    };
}
```

Since `count` is returned via the closure, it must outlive the function. The compiler heap-promotes `count` automatically.

## Decision

**Use Option 1: Closure Struct + Funcref (Wasm GC Native)**

Implement closures as a pair of:

1. **Environment struct** (Wasm GC struct with captured variables as fields)
2. **Function reference** (funcref pointing to implementation function)

### Detailed Design

#### Closure Representation

For each unique closure signature `(params, captures)`, generate:

```wat
;; Environment struct type
(type $ClosureEnv_N (struct
  (field $capture_0 (mut <type>))  ;; One field per captured variable
  (field $capture_1 <type>)        ;; Immutable if not mut
  ...
))

;; Function type (env as first param)
(type $ClosureFn_N (func
  (param $env (ref $ClosureEnv_N))
  (param $param_0 <type>)
  ...
  (result <type>)
))

;; Closure value struct
(type $Closure_N (struct
  (field $env (ref $ClosureEnv_N))
  (field $func (ref $ClosureFn_N))
))
```

#### Closure Creation

```wado
// Source
let count = 42;
let f = |x| { return x + count; };
```

```wat
;; Generated code
;; 1. Allocate environment
(struct.new $ClosureEnv_0
  (local.get $count)  ;; Initialize captured variables
)
(local.set $env)

;; 2. Create closure struct
(struct.new $Closure_0
  (local.get $env)
  (ref.func $closure_impl_0)
)
(local.set $f)
```

#### Closure Invocation

```wado
// Source
f(10)
```

```wat
;; Generated code
(local.get $f)
(struct.get $Closure_0 $env)   ;; Extract environment
(i32.const 10)                 ;; Push argument
(local.get $f)
(struct.get $Closure_0 $func)  ;; Extract function
(call_ref $ClosureFn_0)        ;; Call with env as first param
```

#### Multiple Closures Sharing Environment

```wado
let mut count = 0;
let inc = || { count += 1; };
let get = || { return count; };
```

Both closures share the same environment struct:

```wat
;; Shared environment
(struct.new $ClosureEnv_0
  (i32.const 0)  ;; count
)
(local.set $shared_env)

;; inc closure
(struct.new $Closure_0
  (local.get $shared_env)
  (ref.func $inc_impl)
)

;; get closure
(struct.new $Closure_1
  (local.get $shared_env)
  (ref.func $get_impl)
)
```

When `inc` modifies `count`, `get` sees the updated value because they share the same environment struct.

#### Heap Promotion for Escaping Closures

When a closure escapes (returned from function, stored in struct, etc.), captured variables are heap-promoted:

```wado
fn make_counter() -> Fn() -> i32 with captures[count] {
    let mut count = 0;
    return || {
        count += 1;
        return count;
    };
}
```

**Compilation:**

1. Detect that `count` is captured by a returning closure
2. Allocate environment struct at function start (instead of using locals)
3. All references to `count` become `struct.get`/`struct.set` on the environment
4. Return the closure struct (env + func)

**Alternative for non-escaping closures:**

If a closure doesn't escape (e.g., passed to a function that doesn't store it), the compiler MAY optimize by:

- Using locals for captured variables (no heap allocation)
- Creating the environment struct only when calling the closure

This is an **optimization**, not part of the language semantics.

#### Type System Integration

**In TIR:**

```rust
TypeId::Function {
    params: Vec<TypeId>,
    ret: TypeId,
    captures: Vec<String>,  // Names of captured variables
}
```

**In Wasm:**

Each unique `(params, ret, capture_types)` tuple generates distinct Wasm types.

**Type checking:**

```wado
fn takes_pure(f: Fn(i32) -> i32) { ... }
fn takes_capturing(f: Fn(i32) -> i32 with captures[0]) { ... }

let pure = |x| { return x + 1; };
let capturing = |x| { return x + count; };

takes_pure(pure);         // OK
takes_pure(capturing);    // ERROR: captures not allowed
takes_capturing(pure);    // OK: pure is subtype of capturing
takes_capturing(capturing); // OK
```

**Subtyping rule:**

```
Fn(P) -> R  <:  Fn(P) -> R with captures[...]
```

A pure function can be used where a capturing function is expected.

#### Component Model Boundary

At Component Model boundaries, closures cannot be passed directly (Component Model doesn't support closure types). Instead:

**Option A: Compile error**

```wado
// ERROR: Cannot export/import closures across Component Model boundary
export fn take_callback(f: Fn() -> i32) { ... }
```

**Option B: Resource adapter** (Future work)

Wrap closures as resources for export:

```wit
resource callback {
    call: func() -> s32;
}

export take-callback: func(cb: callback);
```

This requires a resource table mapping handles to closure structs.

**For Wado MVP: Use Option A** (disallow closures at CM boundary). This can be relaxed later.

## Consequences

### Positive

1. **Native Wasm GC representation**: Efficient, garbage collected automatically
2. **Shared mutable state**: Multiple closures can capture and mutate the same variable correctly
3. **Type-safe**: Each closure signature has distinct Wasm types
4. **Matches Rust/Go/Swift semantics**: Familiar to users of these languages
5. **Compatible with value semantics**: Closures capture by reference, consistent with ADR
6. **Optimization-friendly**: Compiler can optimize non-escaping closures to use locals

### Negative

1. **Complex codegen**: Need to generate unique struct types, allocate environment, manage funcref
   - **Mitigation**: Centralize closure codegen in a dedicated module
2. **Type system complexity**: Closure types with captures need careful handling
   - **Mitigation**: Build on existing `FunctionType` with `captures` field
3. **Cannot cross Component Model boundary** (for MVP)
   - **Mitigation**: Provide clear error messages; document workaround (use callbacks via resources)
4. **Indirect function calls**: `call_ref` is slower than direct `call` on some runtimes
   - **Mitigation**: Wasm engines are optimizing `call_ref` performance; closures inherently require indirection

### Implementation Plan

1. **Phase 1: Environment struct generation**
   - Extend codegen to generate struct types for closure environments
   - Implement environment allocation at closure creation sites

2. **Phase 2: Closure value representation**
   - Generate closure struct types (env + funcref)
   - Implement closure creation (struct.new with env + func)

3. **Phase 3: Closure invocation**
   - Generate code for extracting env and func from closure struct
   - Implement `call_ref` with environment as first parameter

4. **Phase 4: Capture analysis**
   - Use existing `TirCapture` analysis from resolver
   - Generate environment fields from captures list

5. **Phase 5: Heap promotion**
   - Detect escaping closures (returned, stored, passed to capturing function)
   - Promote captured variables to environment struct

6. **Phase 6: Type system integration**
   - Implement subtyping for `Fn(P) -> R` <: `Fn(P) -> R with captures[...]`
   - Type check closure passing at call sites

### Future Work

- **Optimization**: Avoid heap allocation for non-escaping closures (use locals + trampoline)
- **Optimization**: Inline small closures at call sites
- **Component Model export**: Support closures at CM boundary via resource adapters
- **Generic closures**: Support `Fn<T>(T) -> T` with type parameters

## References

- [ADR: Value Semantics and Reference Captures](./adr-2026-01-12-value-semantics-and-captures.md)
- [WebAssembly GC Proposal](https://github.com/WebAssembly/gc)
- [Wasm Component Model](https://github.com/WebAssembly/component-model)
- [Rust Closure Implementation](https://doc.rust-lang.org/book/ch13-01-closures.html)
- [Go Closure Implementation](https://golang.org/ref/spec#Function_literals)
