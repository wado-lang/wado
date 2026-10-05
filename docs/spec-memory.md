# Memory Model

This chapter covers how values are copied and shared: value semantics,
references, and how parameters and receivers hold their arguments. Memory is
garbage-collected, so a program writes no lifetimes and frees nothing.

## Value Semantics

Assignment, parameter passing, and return all perform a deep copy of the value. Primitives, structs, variants, `String`, and `List<T>` all follow this rule.

The copy stops at a reference. A `&T` or `&mut T`, wherever it appears, is copied as a reference, and the copy aliases the same value. So a `Slice<T>` or a `StrSlice`, which is a struct holding a reference to its backing array, shares its elements with the value it views.

A resource is a handle to something the host owns, not a Wado value, so it is never deep-copied. An affine resource is move-only: assignment, parameter passing, and return move it, and the source is unusable afterwards (see [Resource Ownership](./spec-components.md#resource-ownership)).

<!-- {"fixture":"spec_memory_copy.wado"} -->

```wado
struct Point { x: i32, y: i32 }

let a = Point { x: 1, y: 2 };
let mut b = a;   // b is a deep copy of a
b.x = 10;        // does not affect a
assert a.x == 1;
```

In-place mutation through a parameter binding (field writes, method calls, index writes) operates on the callee's local copy and is not visible to the caller. To allow callee-side mutation, pass a reference explicitly:

<!-- {"fixture":"spec_memory_values.wado"} -->

```wado
fn translate(p: &mut Point, dx: i32, dy: i32) {
    p.x += dx;   // visible to caller (reference)
    p.y += dy;
}

test {
    let mut p = Point { x: 1, y: 2 };
    translate(&mut p, 10, 20);
    assert p.x == 11 && p.y == 22;
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

A reference gives indirect access to a value. Unlike Rust, Wado has no borrow
checker: the garbage collector keeps every referent alive, so a reference never
dangles.

### Basic Reference Syntax

<!-- {"fixture":"spec_memory_values.wado"} -->

```wado
let x = 42;
let r = &x;           // Immutable reference
let v = *r;           // Dereference
assert v == 42;

let mut y = 0;
let mr = &mut y;      // Mutable reference
*mr = 10;             // Assign through reference
assert y == 10;
```

A reference is never null, and there is no arithmetic on it. A reference that
may be absent is written `Option<&T>`.

### Reference to Reference

References can be nested arbitrarily:

<!-- {"fixture":"spec_memory_values.wado"} -->

```wado
let x = 42;
let r = &x;           // &i32
let rr = &r;          // &&i32
let val = **rr;       // double dereference
assert val == 42;
```

### Automatic Coercion (`&mut` to `&`)

Mutable references automatically coerce to immutable references when needed:

<!-- {"fixture":"spec_memory_values.wado"} -->

```wado
fn read_value(r: &i32) -> i32 {
    return *r;
}

test {
    let mut x = 10;
    assert read_value(&mut x) == 10;   // OK: &mut i32 coerces to &i32
}
```

### Returning References to Local Variables

A reference to a local variable stays valid after the function returns:

<!-- {"fixture":"spec_memory_values.wado"} -->

```wado
fn make_ref() -> &i32 {
    let x = 42;
    return &x;  // OK in Wado (x is GC-managed and stays alive)
}

test {
    let r = make_ref();
    assert *r == 42;
}
```

### Multiple Mutable References

Wado allows multiple mutable references to the same value:

<!-- {"fixture":"spec_memory_values.wado"} -->

```wado
let mut x = 10;
let r1 = &mut x;
let r2 = &mut x;  // OK in Wado (no borrow checker)

*r1 = 20;
*r2 = 30;
assert x == 30;
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

### Mutable References to Fields and Elements

A primitive, `enum`, `flags` or `fn` field or element keeps nothing that a write
through a `&mut` could reach, since assignment replaces it. So taking `&mut` of
one is an error. That holds whether it is written `&mut x.f` or `&mut xs[i]`, or
taken implicitly by a `&mut self` receiver. A `&mut` of a local is fine, since it
writes to the variable itself.

A `variant` field or element admits `&mut`. The reference points to a copy of
the variant that shares its payload with the original. So a mutation reached
through [match ergonomics](./spec-patterns.md#match-ergonomics), such as
`if let A(b) = r { b.n = 1; }`, lands. Replacing the whole value through the
reference would not, so it is an error.

These rules for references to replace-on-assign values are not settled. They are
being relaxed toward what Rust allows, so expect them to change.

### Reference Identity

`==` and `!=` on two references compare the values they point to, as in Rust
(see [Eq](./spec-standard-traits.md#eq---equality)). The prelude function
`ref_eq(a: &T, b: &T) -> bool` compares identity instead: whether the two
references point to one place. A place is where a value is stored: a variable,
a field, an element.

Identity is guaranteed in one direction only. Two references to one place are
always `ref_eq`: `&x` taken twice of one variable, or a reference and a copy of
it. References to distinct places may also be `ref_eq`, because the copies value
semantics promise are as-if: the implementation may store equal content once,
by eliding a copy or by interning a constant `String` or `List`. Whether it
does is [unspecified](./spec-overview.md#behavior-classes), so a `ref_eq` that
is true only by such sharing is unpredictable. Java's `==` on
strings behaves the same way.

<!-- {"fixture": "spec_memory_ref_identity.wado"} -->

```wado
let mut xs: List<i32> = [1, 2, 3];
let ys: List<i32> = [1, 2, 3];
let zs = xs;
assert &xs == &ys;              // equal values
assert ref_eq(&xs, &xs);        // always
ref_eq(&xs, &ys);               // false or true: the two may be stored once
ref_eq(&xs, &zs);               // false or true: the copy may be elided
```

A `&` to a `List` element or a struct field of a type that assignment replaces
(a primitive, `enum`, `flags`, `variant` or `fn`) points to a copy of the value,
taken where the `&` is written. It does not see a later assignment to the
element or field, and two such references are two places:

<!-- {"fixture": "spec_memory_ref_identity.wado"} -->

```wado
let r = &xs[0];
xs[0] = 9;
assert *r == 1;
ref_eq(&xs[0], &xs[0]);         // false or true: each `&` takes its own copy
```

A closure's identity stays unobservable. `ref_eq` takes references only, and a
reference to a closure points to the place holding it, not to the closure:

<!-- {"fixture": "spec_memory_ref_identity.wado"} -->

```wado
let f = || 1;
let g = f;
assert ref_eq(&f, &f);          // always
ref_eq(&f, &g);                 // false or true: two places holding one closure
```

## Parameters

### Method Receiver: `self` by Value

A method receiver is `&self` or `&mut self`. Bare `self` (by value) is allowed only on a resource, on an aggregate that holds one, or on a generic type, since its type arguments may be resources (`Option<T>::unwrap(self)`):

<!-- {"fixture":"spec_memory_values.wado"} -->

```wado
impl Point {
    fn sum(&self) -> i32 { return self.x + self.y; }  // OK: immutable reference
    fn reset(&mut self) { self.x = 0; self.y = 0; }   // OK: mutable reference
}

test {
    let mut p = Point { x: 1, y: 2 };
    assert p.sum() == 3;
    p.reset();
    assert p.sum() == 0;
}
```

`Point` holds no resource, so `self` by value on it is an error:

<!-- {"fixture":"spec_memory_self_by_value.wado"} -->

```wado
impl Point {
    fn consume(self) -> i32 { return self.x; }
}
```

A by-value `self` is passed as any parameter is. A receiver holding an affine resource moves into the method, so the caller's binding cannot be used afterward, which is how the resource is consumed (see [Resource Ownership](./spec-components.md#resource-ownership)). Any other receiver is copied, so `Option<i32>::unwrap` leaves its binding usable.

### `mut` Parameters

A parameter declared `mut` may be written in the body, as a `let mut` binding
may ([Variable Mutability](./spec-expressions.md#variable-mutability)):

<!-- {"fixture":"spec_memory_values.wado"} -->

```wado
fn increment(mut n: i32) -> i32 {
    n += 1;   // mutates the local copy
    return n;
}

fn normalize(mut s: String) -> String {
    s = s.to_ascii_uppercase();  // rebinds local binding
    return s;
}

test {
    assert increment(1) == 2 && normalize("ab") == "AB";
}
```

Writing a `mut` parameter changes only the callee's copy ([Value Semantics](#value-semantics)). A `&mut T` parameter holds a copy of the reference, so a write through it (`*p = v`) reaches the referent.

<!-- {"fixture":"spec_memory_values.wado"} -->

```wado
fn countdown(mut n: i32) with Stdout {
    while n > 0 {
        println(`${n}`);
        n -= 1;         // only modifies the local copy
    }
}

test {
    let x = 3;
    countdown(x);
    assert x == 3;      // every parameter is passed by value
}
```

Closures also support `mut` parameters:

<!-- {"fixture":"spec_memory_values.wado"} -->

```wado
let add_one = |mut n: i32| {
    n += 1;
    return n;
};
assert add_one(1) == 2;
```

Without `mut`, any assignment to a parameter is a compile error:

<!-- {"fixture":"spec_memory_assign_immutable_param.wado"} -->

```wado
fn bad(n: i32) {
    n = 10;
}
```
