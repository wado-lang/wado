# Memory Model

## Core Principles

- Wasm-GC based: Garbage collection delegated to runtime
- Lifetime inference: No explicit lifetime annotations required
- Value semantics: a value is deeply copied on assignment, parameter passing, and return. References (`&T`, `&mut T`) share state instead, and an affine resource moves

## Value Semantics

Assignment, parameter passing, and return all perform a deep copy of the value. Primitives, structs, `String`, and `List<T>` all follow this rule uniformly. There are two exceptions. Reference types (`&T`, `&mut T`) alias the underlying value. An affine resource is move-only: assignment, parameter passing, and return move it, and the source is unusable afterwards (see [Resource Ownership](./spec-components.md#resource-ownership)).

```wado
struct Point { x: i32, y: i32 }

let a = Point { x: 1, y: 2 };
let mut b = a;   // b is a deep copy of a
b.x = 10;        // does not affect a
assert a.x == 1;
```

In-place mutation through a parameter binding (field writes, method calls, index writes) operates on the callee's local copy and is not visible to the caller. To allow callee-side mutation, pass a reference explicitly:

```wado
fn translate(p: &mut Point, dx: i32, dy: i32) {
    p.x += dx;   // visible to caller (reference)
    p.y += dy;
}
```

These semantics are as-if. A program may rely on the value each expression
denotes; it may not rely on the number of copies performed to produce it.

A program never chooses where a value lives. There is no stack or heap to pick
between. A value needs no annotation to outlive its scope, because the garbage
collector keeps alive every value a reference can reach.

A closure that outlives the scope that declared it needs no rule of its own:
the collector keeps what it captured alive. How a closure copies is in
[Closures Are Values](./spec-functions.md#closures-are-values).

A call across a component boundary copies its arguments and its result, so the
two components never share a value's storage. A resource crosses as a handle
([Type Mapping at Component Boundaries](./spec-components.md#type-mapping-at-component-boundaries)).

Rationale: [WEP: Value Semantics and Reference Retention](./wep-2026-01-12-value-semantics-and-retention.md).

## Reference Types

References in Wado provide indirect access to values. Unlike Rust, Wado uses a GC-based memory model with no borrow checker, enabling simpler semantics at the cost of runtime overhead.

### Basic Reference Syntax

```wado
let x = 42;
let r = &x;           // Immutable reference
let v = *r;           // Dereference

let mut y = 0;
let mr = &mut y;      // Mutable reference
*mr = 10;             // Assign through reference
```

A reference is never null, and there is no arithmetic on it. A reference that
may be absent is written `Option<&T>`.

### Reference to Reference

References can be nested arbitrarily:

```wado
let x = 42;
let r = &x;           // &i32
let rr = &r;          // &&i32
let val = **rr;       // 42 (double dereference)
```

### Automatic Coercion (`&mut` to `&`)

Mutable references automatically coerce to immutable references when needed:

```wado
fn read_value(r: &i32) -> i32 {
    return *r;
}

let mut x = 10;
read_value(&mut x);   // OK: &mut i32 coerces to &i32
```

### Key Differences from Rust (GC-Based Memory Model)

| Aspect                 | Rust                       | Wado                     |
| ---------------------- | -------------------------- | ------------------------ |
| Memory management      | Ownership + borrow checker | Garbage collection       |
| Multiple mutable refs  | Not allowed                | Allowed                  |
| Returning local refs   | Not allowed (dangling)     | Allowed (GC keeps alive) |
| Reference to reference | `&&T` (rare)               | `&&T` (fully supported)  |
| Lifetime annotations   | Required                   | Not needed               |
| Borrow checking        | Compile-time               | None; resources move     |

### Returning References to Local Variables

Because Wado uses garbage collection, references to local variables remain valid after the function returns:

```wado
fn make_ref() -> &i32 {
    let x = 42;
    return &x;  // OK in Wado (x is GC-managed and stays alive)
}

let r = make_ref();
println(`${*r}`);  // Works: prints "42"
```

This would be a dangling pointer error in Rust, but is safe in Wado due to garbage collection.

### Multiple Mutable References

Wado allows multiple mutable references to the same value:

```wado
let mut x = 10;
let r1 = &mut x;
let r2 = &mut x;  // OK in Wado (no borrow checker)

*r1 = 20;
*r2 = 30;
```

### Reference Retention

A function may keep a reference parameter past its return: it may store it in a
global, or write it through a `&mut` parameter the caller still holds. It may
also return a reference into a parameter's storage. Each is safe, because the
referent is GC-managed and cannot dangle.

A function with a body declares nothing about what it keeps: that is inferred
from the body. A function type carries no such declaration, and neither does a
closure, so a function value is called the same way whatever the function
keeps. Retention is not an effect either. It grants no authority and no handler
intercepts it, so it has no place in a `with` clause.

A declaration with no body has nothing to infer from, so it states what it keeps with
[`#[retain(...)]` / `#[result(...)]`](./spec-attributes.md#retain--result).

Rationale: [WEP: Value Semantics and Reference Retention](./wep-2026-01-12-value-semantics-and-retention.md).

### Reference Identity

`==` and `!=` on two references compare the values they point to, as in Rust
(see [Eq](./spec-traits.md#eq---equality)). The prelude function
`ref_eq(a: &T, b: &T) -> bool` compares identity instead: whether the two
references point to one place. A place is where a value is stored: a variable,
a field, an element.

Identity is guaranteed in one direction only. Two references to one place are
always `ref_eq`: `&x` taken twice of one variable, or a reference and a copy of
it. References to distinct places may also be `ref_eq`, because the copies value
semantics promise are as-if: the implementation may store equal content once,
by eliding a copy or by interning a constant `String` or `List`. Whether it
does can change with the optimization level and with the Wado version, so a
`ref_eq` that is true only by such sharing is unpredictable. Java's `==` on
strings behaves the same way.

```wado
let mut xs: List<i32> = [1, 2, 3];
let ys: List<i32> = [1, 2, 3];
let zs = xs;
&xs == &ys;                     // true: equal values
ref_eq(&xs, &xs);               // always true
ref_eq(&xs, &ys);               // false or true: the two may be stored once
ref_eq(&xs, &zs);               // false or true: the copy may be elided
```

A `&` to a `List` element or a struct field of a type that assignment replaces
(a primitive, `enum`, `flags`, `variant` or `fn`) points to a copy of the value,
taken where the `&` is written. It does not see a later assignment to the
element or field, and two such references are two places:

```wado
let r = &xs[0];
xs[0] = 9;
*r;                             // 1
ref_eq(&xs[0], &xs[0]);         // false or true: each `&` takes its own copy
```

A closure's identity stays unobservable. `ref_eq` takes references only, and a
reference to a closure points to the place holding it, not to the closure:

```wado
let f = || 1;
let g = f;
ref_eq(&f, &f);                 // always true
ref_eq(&f, &g);                 // false or true: two places holding one closure
```

## Parameters

### Method Receiver: `self` by Value

A method receiver is `&self` or `&mut self`. Bare `self` (by value) is allowed only on a resource, on an aggregate that holds one, or on a generic type, since its type arguments may be resources (`Option<T>::unwrap(self)`):

```wado
impl Point {
    fn sum(&self) -> i32 { ... }          // OK: immutable reference
    fn reset(&mut self) { ... }           // OK: mutable reference
    // fn consume(self) -> i32 { ... }    // ERROR: `Point` holds no resource
}
```

A by-value `self` moves the receiver into the method, so the caller's binding cannot be used afterward. That is how an affine resource is consumed (see [Resource Ownership](./spec-components.md#resource-ownership)). A value type has nothing to consume, so `self` by value on one is a compile error.

### `mut` Parameters

A parameter can be declared `mut` to allow the function body to reassign it:

```wado
fn increment(mut n: i32) -> i32 {
    n += 1;   // mutates the local copy
    return n;
}

fn normalize(mut s: String) -> String {
    s = s.to_ascii_uppercase();  // rebinds local binding
    return s;
}
```

The `mut` keyword grants write access to the local parameter binding inside the function. The parameter holds the callee's own copy ([Value Semantics](#value-semantics)), so neither reassignment (`p = new_value`) nor in-place mutation reaches the caller.

```wado
fn countdown(mut n: i32) with Stdout {
    while n > 0 {
        println(`${n}`);
        n -= 1;         // only modifies the local copy
    }
}

let x = 3;
countdown(x);
// x is still 3 — every parameter is passed by value
```

Closures also support `mut` parameters:

```wado
let add_one = |mut n: i32| {
    n += 1;
    return n;
};
```

Without `mut`, any assignment to a parameter is a compile error:

```wado
fn bad(n: i32) {
    n = 10;  // Error: cannot assign to immutable variable 'n'
}
```
