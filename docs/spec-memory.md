# Memory Model

## Core Principles

- Wasm-GC based: Garbage collection delegated to runtime
- Lifetime inference: No explicit lifetime annotations required
- Value semantics: a value is deeply copied on assignment, parameter passing, and return. References (`&T`, `&mut T`) share state instead, and an affine resource moves

## Value Semantics

See [WEP: Value Semantics and Reference Retention](./wep-2026-01-12-value-semantics-and-retention.md).

Assignment, parameter passing, and return all perform a deep copy of the value. Primitives, structs, `String`, and `List<T>` all follow this rule uniformly. There are two exceptions. Reference types (`&T`, `&mut T`) alias the underlying value. An affine resource is move-only: assignment, parameter passing, and return move it, and the source is unusable afterwards (see [Resource linearity](./spec-components.md#resource-linearity)).

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

### Reference Identity

`==` and `!=` on two references compare the values they point to, as in Rust
(see [Eq](./spec-traits.md#eq---equality)). The prelude function
`ref_eq(a: &T, b: &T) -> bool` compares identity instead: whether the two
references point to one place. A place is where a value is stored: a variable,
a field, an element. For a `struct`, `List` or `String`, assignment copies the
object, so each place holds its own and the object is the place. For a type
that assignment replaces, such as a primitive, `variant` or `fn`, copies share
one value, so the place is the variable, field or element holding it.

Identity is guaranteed in one direction only. Two references to one place are
always `ref_eq`. Two references to distinct places of identical content may also
be `ref_eq`: as an optimization, the implementation may make them one place, as
it does when it interns a constant `String` or `List`. Whether it does can
change with the optimization level and with the Wado version, so a `ref_eq`
that is true only by such merging is unpredictable. Java's `==` on strings
behaves the same way. An identity comparison that should be true is never false.

```wado
let xs: List<i32> = [1, 2, 3];
let ys: List<i32> = [1, 2, 3];
&xs == &ys;                     // true: equal values
ref_eq(&xs, &xs);               // always true
ref_eq(&xs, &ys);               // false or true: the two may be one place

let a: String = "abc";
let b: String = "abc";
ref_eq(&a, &b);                 // false or true, likewise
```

A closure's identity stays unobservable. `ref_eq` takes references only, and a
reference to a closure points to the place holding it:

```wado
let f = || 1;
let g = f;
ref_eq(&f, &f);                 // true
ref_eq(&f, &g);                 // false: two places holding one closure
```

### Design Trade-offs

- Simplicity: No lifetime annotations or borrow checker errors
- Flexibility: Can freely share and modify references
- Cost: Runtime overhead from garbage collection
- Safety: Memory safety guaranteed by GC, not compile-time checks

### Method Receiver: `self` by Value

A method receiver is `&self` or `&mut self`. Bare `self` (by value) is allowed only on a resource, or on an aggregate that holds one:

```wado
impl Point {
    fn sum(&self) -> i32 { ... }          // OK: immutable reference
    fn reset(&mut self) { ... }           // OK: mutable reference
    // fn consume(self) -> i32 { ... }    // ERROR: `self` by value is only allowed on a resource
}
```

A by-value `self` moves the receiver into the method, so the caller's binding cannot be used afterward. That is how an affine resource is consumed (see [Resource linearity](./spec-components.md#resource-linearity)). A value type has nothing to consume, so `self` by value on one is a compile error. See [WEP: Resource Ownership](./wep-2026-05-21-resource-ownership.md).

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

The `mut` keyword grants write access to the local parameter binding inside the function. Wado uses value semantics for every parameter: every value is deeply copied when passed to a function. This applies uniformly to primitives, structs, `String`, and `List<T>`. References (`&T`, `&mut T`) are the exception: they share state with the caller. An affine resource is never copied: passing it moves it. Inside the callee, reassignment (`p = new_value`) and in-place mutation operate on the callee's local copy and are not visible to the caller. In-place mutation covers field writes (`p.x = ...`), method calls (`s.push_str("!")`, `arr.push(0)`), and index writes (`arr[0] = ...`). To let the callee mutate the caller's value, declare the parameter as `&mut T` and pass a `&mut`-reference at the call site.

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
