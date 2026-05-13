# WEP: Closure Implementation

## Context

Wado supports closure syntax (parsed but not codegen yet):

```wado
let f = |x| { return x + 1; };
let g = || { return count; };  // Captures outer variable 'count'
```

The language design requires:

1. First-class functions (closures can be passed as values)
2. Capture by reference (per [WEP: Value Semantics and Reference Stores](./wep-2026-01-12-value-semantics-and-stores.md))
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

**Conceptual representation (desugared Wado):**

```wado
// Original code:
let mut count = 0;
let inc = || { count += 1; };
let get = || { return count; };

// Desugared to (conceptually):
struct ClosureEnv_0 {
    mut count: i32,
}

fn closure_inc_impl(env: &mut ClosureEnv_0) {
    env.count += 1;
}

fn closure_get_impl(env: &ClosureEnv_0) -> i32 {
    return env.count;
}

struct Closure_Inc {
    env: &mut ClosureEnv_0,
    func: FnRef(&mut ClosureEnv_0),
}

struct Closure_Get {
    env: &ClosureEnv_0,
    func: FnRef(&ClosureEnv_0) -> i32,
}

// Shared environment (allocated once)
let shared_env = ClosureEnv_0 { count: 0 };

// Both closures reference the same environment
let inc = Closure_Inc {
    env: &mut shared_env,
    func: closure_inc_impl,
};

let get = Closure_Get {
    env: &shared_env,
    func: closure_get_impl,
};

// Calling closures:
inc.func(inc.env);      // count becomes 1
inc.func(inc.env);      // count becomes 2
println(get.func(get.env));  // Prints "2" ✓
```

**Key insight:** Both `inc` and `get` share the same `shared_env` struct, so mutations in one closure are visible to the other.

**Wasm representation:**

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

**Representation (desugared Wado):**

```wado
// Original closure: |x| { return x + count; }
// Generated implementation function:
fn closure_impl(x: i32, count_capture: i32) -> i32 {
    return x + count_capture;
}

// Trampoline created at closure creation site:
// let f = |x| { return x + count; };
fn trampoline_0(x: i32) -> i32 {
    return closure_impl(x, count);  // count captured by value
}

// Closure value: funcref to trampoline_0
let f = trampoline_0;
```

**Closure value:** Just a function reference (funcref)

**Pros:**

- Simplest representation: closure = funcref
- Compatible with Component Model function types directly
- No need for environment struct types
- Good for closures with few captures

**Cons:**

- **Cannot share mutable state** (without additional indirection):
  - Each trampoline captures by value, not reference
  - Multiple closures capturing the same variable get independent copies
  - Mutations don't propagate between closures

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

**Could this work with references?**

Yes, but requires **implicit heap allocation and wrapping**:

```wado
// Compiler must transform:
let mut count = 0;
let inc = || { count += 1; };

// Into:
struct Cell<T> { mut value: T }
let count_cell = Cell { value: 0 };

// Trampoline captures reference to cell:
fn trampoline_inc() {
    return closure_inc_impl(&mut count_cell);
}

// All accesses to 'count' become 'count_cell.value'
```

**Issues with this approach:**

1. **Implicit heap allocation**: Every captured mutable variable requires a heap-allocated Cell
2. **Hidden complexity**: User writes `count` but compiler generates `count_cell.value` everywhere
3. **Reference lifetime complexity**: Need to track which variables escape via closures
4. **Invasive transformation**: All references to captured variables change, not just in closures

This is essentially **re-implementing Option 1** (environment struct) but with:

- More implicit magic (hidden Cell wrappers)
- Less efficient (separate Cell per variable instead of one struct for all captures)
- More confusing semantics (why does `count` behave differently when captured?)

Therefore, **Option 1 is cleaner**: explicitly create an environment struct with all captures, rather than wrapping each variable individually.

#### Option 3: Defunctionalization

Convert closures to an enum of closure types and an interpreter function.

**Conceptual representation (desugared Wado):**

```wado
// Original closures:
let mut count = 0;
let inc = || { count += 1; };
let get = || { return count; };
let add = |x| { return x + count; };

// Desugared to:
enum ClosureKind {
    Inc { env: &mut CountEnv },
    Get { env: &CountEnv },
    Adder { env: &CountEnv, x: i32 },
}

struct CountEnv {
    mut count: i32,
}

// Single interpreter function for all closures
fn call_closure(kind: ClosureKind) -> i32 {
    match kind {
        ClosureKind::Inc { env } => {
            env.count += 1;
            return 0;  // unit
        }
        ClosureKind::Get { env } => {
            return env.count;
        }
        ClosureKind::Adder { env, x } => {
            return x + env.count;
        }
    }
}

// Closures are enum values
let shared_env = CountEnv { count: 0 };
let inc = ClosureKind::Inc { env: &mut shared_env };
let get = ClosureKind::Get { env: &shared_env };
```

**Wasm representation:**

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

**Conceptual representation (desugared Wado):**

```wado
// Original closure:
let mut count = 0;
let inc = || { count += 1; };

// Desugared to use resource handles:
struct ClosureEnv {
    mut count: i32,
}

// Global handle table (runtime-managed)
static mut CLOSURE_TABLE: TreeMap<i32, (ClosureEnv, FnRef)> = TreeMap::new();
static mut NEXT_HANDLE: i32 = 0;

fn create_closure(env: ClosureEnv, func: FnRef) -> i32 {
    let handle = NEXT_HANDLE;
    NEXT_HANDLE += 1;
    CLOSURE_TABLE[handle] = (env, func);
    return handle;  // Return handle as closure value
}

fn call_closure(handle: i32) {
    let (env, func) = CLOSURE_TABLE[handle];
    func(env);
}

fn drop_closure(handle: i32) {
    CLOSURE_TABLE.remove(handle);  // Manual cleanup
}

// Usage:
let env = ClosureEnv { count: 0 };
let inc_handle = create_closure(env, inc_impl);

call_closure(inc_handle);  // Indirect lookup in table
drop_closure(inc_handle);  // Manual drop required
```

**WIT representation:**

```wit
// In WIT
resource closure-env {
    call: func(arg: s32) -> s32;
}
```

**Wasm representation:**

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

| Language           | Approach                                               | Wasm Target         |
| ------------------ | ------------------------------------------------------ | ------------------- |
| **Rust**           | Closure traits (Fn, FnMut, FnOnce) + struct for env    | Struct + funcref    |
| **Go**             | Closure struct with env pointer                        | Struct + funcref    |
| **Swift**          | Closure = function + captured vars in struct           | Struct + funcref    |
| **OCaml**          | Closure = code pointer + environment block             | Similar to struct   |
| **Scheme**         | First-class continuations, heap-allocated environments | GC struct + funcref |
| **AssemblyScript** | Closure = funcref + trampolines                        | Trampolines (no GC) |

Most languages with GC targeting Wasm use **Option 1** (struct + funcref) because it provides:

- Shared mutable state
- Efficient representation
- Native GC support

### Integration with `captures[...]` Tracking

Per the [Value Semantics WEP](./wep-2026-01-12-value-semantics-and-stores.md), closures that capture variables require `captures[...]` annotation in function types:

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
// Source
let mut count = 0;
let inc = || { count += 1; };
let get = || { return count; };
```

**Desugared (conceptual):**

```wado
// Environment struct definition
struct ClosureEnv_0 {
    mut count: i32,
}

// Implementation functions
fn inc_impl(env: &mut ClosureEnv_0) {
    env.count += 1;
}

fn get_impl(env: &ClosureEnv_0) -> i32 {
    return env.count;
}

// Closure struct types (simplified)
struct Closure_Inc { env: &mut ClosureEnv_0, func: FnRef }
struct Closure_Get { env: &ClosureEnv_0, func: FnRef }

// Shared environment (allocated once)
let shared_env = ClosureEnv_0 { count: 0 };

// Both closures reference the same environment
let inc = Closure_Inc { env: &mut shared_env, func: inc_impl };
let get = Closure_Get { env: &shared_env, func: get_impl };

// Usage:
inc.func(inc.env);  // shared_env.count = 1
inc.func(inc.env);  // shared_env.count = 2
println(get.func(get.env));  // Prints 2 ✓
```

**Wasm representation:**

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
// Source
fn make_counter() -> Fn() -> i32 with captures[count] {
    let mut count = 0;
    return || {
        count += 1;
        return count;
    };
}
```

**Desugared (with heap promotion):**

```wado
// Environment struct (heap-allocated)
struct ClosureEnv_Counter {
    mut count: i32,
}

// Implementation function
fn counter_impl(env: &mut ClosureEnv_Counter) -> i32 {
    env.count += 1;
    return env.count;
}

// Closure type
struct Closure_Counter {
    env: &mut ClosureEnv_Counter,
    func: FnRef,
}

fn make_counter() -> Closure_Counter {
    // Heap-allocate environment at function start
    let env = ClosureEnv_Counter { count: 0 };

    // Return closure that owns the environment
    return Closure_Counter {
        env: &mut env,
        func: counter_impl,
    };
    // env outlives the function because it's returned via closure
}

// Usage:
let c = make_counter();
println(c.func(c.env));  // 1
println(c.func(c.env));  // 2
println(c.func(c.env));  // 3
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

#### Closure Mutability

Closures are distinguished by whether they mutate their captures:

| Type             | Description              |
| ---------------- | ------------------------ |
| `fn(T) -> U`     | Pure/stateless closure   |
| `fn mut(T) -> U` | Stateful/mutable closure |

**Comparison with Rust:**

| Wado             | Rust equivalent          |
| ---------------- | ------------------------ |
| `fn(T) -> U`     | `&dyn Fn(T) -> U`        |
| `fn mut(T) -> U` | `&mut dyn FnMut(T) -> U` |

Unlike Rust, Wado does not have bare function pointers (`fn(T) -> U` in Rust). All callable values are closures (reference types), possibly with empty captures. This simplifies the type system since functions without captures are just closures with empty functor structs.

**Syntax:**

```wado
// Pure closure (default)
let double = |x: i32| x * 2;
// Type: fn(i32) -> i32

// Stateful closure (explicit &mut prefix)
let mut count = 0;
let counter = &mut || {
    count += 1;
    count
};
// Type: fn mut() -> i32
```

**Compiler enforcement:**

```wado
let mut count = 0;

// ERROR: closure mutates capture, must use &mut
let f = || { count += 1; count };

// OK: explicit mutable closure
let f = &mut || { count += 1; count };

// OK: pure closure (reads only)
let f = || count + 1;
```

**Subtyping:**

`fn(T) -> U` coerces to `fn mut(T) -> U`, but not vice versa.

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

#### Canonical Closure as Vtable

A closure value that escapes its declaring scope — passed as a `fn(...)` argument, stored in a struct field, returned, assigned to a global, or rebound to another local — is wrapped in a canonical, signature-keyed struct so any holder of an `Fn<N, Ret>` value can dispatch through a uniform shape. The lower-phase escape analysis decides per local: a closure local that only appears as the callee of `IndirectCall` / receiver of `MethodCall` keeps the specialised `&__Closure_N` form; every other position forces canonicalisation.

The canonical struct comes in two shapes, selected per-`(N, Ret)` by a pre-WIR scan of `Fn<N, Ret>^Inspect[Alt]` call sites:

Slim shape (default — `Fn::call` only):

```wat
(type $CanonicalClosure_K (struct
  (field $env  (ref null struct))
  (field $func (ref $canonical_fn_K))))
```

Inspectable shape (`Fn^Inspect` / `Fn^InspectAlt` referenced for this signature):

```wat
(type $canonical_inspectable_base (struct
  (field $env         (ref null struct))
  (field $inspect     (ref $canonical_callback_fn))
  (field $inspect_alt (ref $canonical_callback_fn))))

(type $CanonicalClosure_K (sub $canonical_inspectable_base (struct
  (field $env         (ref null struct))
  (field $inspect     (ref $canonical_callback_fn))
  (field $inspect_alt (ref $canonical_callback_fn))
  (field $func        (ref $canonical_fn_K)))))
```

The subtype keeps the shared prefix — `env`, `inspect`, `inspect_alt` — and adds `func` last. `$canonical_callback_fn` has signature `(param $env (ref null struct)) (param $f (ref null struct))` (uniform across all signatures); `$canonical_fn_K` has signature `(param $env (ref null struct)) (param $p_0 ...) ... (result ...)` (per-`K`, typed). The shared base means a single dispatch stub serves every parameter shape with the same `(N, Ret)`: distinct function types like `fn(i32) -> i32` and `fn(String) -> i32` cast to one common type, eliminating any need for per-signature dispatch tables.

Per-literal wrappers (registered in WIR build for every functor `N` whose signature is inspectable):

1. `__closure_wrapper_N` — casts `env` to `(ref $__Closure_N)` and forwards to `__call`.
2. `__closure_inspect_wrapper_N` — casts `env`, calls `__Closure_N^Inspect::inspect`.
3. `__closure_inspect_alt_wrapper_N` — casts `env`, calls `__Closure_N^InspectAlt::inspect_alt`.

Trait dispatch from the generic `Fn<N, Ret>^InspectAlt::inspect_alt` impl:

```wat
;; Fn<N, Ret>^InspectAlt::inspect_alt(self, f)
(local $b (ref $canonical_inspectable_base))
(local.set $b (ref.cast (ref $canonical_inspectable_base) (local.get $self)))
(call_ref $canonical_callback_fn
  (struct.get $canonical_inspectable_base $env         (local.get $b))
  (local.get $f)
  (struct.get $canonical_inspectable_base $inspect_alt (local.get $b)))
```

The dispatch stub is auto-derived as a `FunctionKind::FnCanonicalDispatch` TIR placeholder with no body — WIR build supplies the instructions above. A bodyless TIR function is naturally skipped by the inliner, monomorphisation, and other body walkers, so the placeholder costs nothing during optimisation.

Programs that never inspect closures emit the slim shape and incur no extra fields, wrappers, or source-string constants. Programs that inspect closures of some `(N, Ret)` pay two refs per canonical value of that signature plus per-literal wrappers and source-string constants for the affected literals only. The `__Closure_N^Inspect[Alt]` impls are TIR-rooted from `ClosureToCanonical` only when the corresponding `(N, Ret)` is inspected.

The specialised path (closure local stays as `&__Closure_N`) does not use the vtable: a redirect at the lowering stage rewrites `Fn<N, Ret>^Inspect[Alt]` calls on known-local receivers to direct calls on `__Closure_N^Inspect[Alt]`, and standard DCE removes those impls when unused.

## Consequences

### Positive

1. **Native Wasm GC representation**: Efficient, garbage collected automatically
2. **Shared mutable state**: Multiple closures can capture and mutate the same variable correctly
3. **Type-safe**: Each closure signature has distinct Wasm types
4. **Matches Rust/Go/Swift semantics**: Familiar to users of these languages
5. **Compatible with value semantics**: Closures capture by reference, consistent with WEP
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
- **Optimization**: Auto-switch between type-erased (`fn(T) -> U`) and monomorphized (generic `F`) forms based on whether closure escapes. This would enable inlining for iterator chains like `arr.iter().map(|x| x * 2).collect()`.
- **Component Model export**: Support closures at CM boundary via resource adapters
- **Generic closures**: Support `Fn<T>(T) -> T` with type parameters

## References

- [WEP: Value Semantics and Reference Stores](./wep-2026-01-12-value-semantics-and-stores.md)
- [WebAssembly GC Proposal](https://github.com/WebAssembly/gc)
- [Wasm Component Model](https://github.com/WebAssembly/component-model)
- [Rust Closure Implementation](https://doc.rust-lang.org/book/ch13-01-closures.html)
- [Go Closure Implementation](https://golang.org/ref/spec#Function_literals)
