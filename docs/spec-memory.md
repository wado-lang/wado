# Memory Model

This chapter covers how values are copied and shared: value semantics,
references, and how parameters and receivers hold their arguments. Values are
garbage-collected, so a program writes no lifetimes and frees nothing. The last
section covers the linear memory beside them.

## Value Semantics

Assignment, parameter passing, and return all perform a deep copy of the value. Primitives, structs, variants, `String`, and `List<T>` all follow this rule.

The copy stops at a reference. A `&T`, wherever it appears, is copied as a reference, and the copy aliases the same value. So a `Slice<T>` or a `StrSlice`, which is a struct holding a reference to its backing array, shares its elements with the value it views. It stops at a [shared cell](#shared-cells) too: a copy of a cell is the same cell. A `&mut T` is not copied at all: it [moves](#writable-references).

A resource is a handle to something the host owns, not a Wado value, so it is never deep-copied. A value of an [affine type](./spec-affine.md) is moved, never copied, and a reference to one names the value itself.

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

A write must land in storage a variable holds, directly or through a
reference. A by-value call's result, or any other value no variable holds, is a
temporary: a write into it would be discarded with it, so it is a compile error.
`get().a = 1` is refused; `get_mut().a = 1` writes through the `&mut` that
`get_mut` returns.

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

let mut p = Point { x: 0, y: 0 };
let mr = &mut p;      // Mutable reference
mr.x = 10;            // Write through reference
assert p.x == 10;
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

A reference is a value that assignment replaces, as a primitive is. So a write
through a `&mut &T` makes the variable holding the reference refer elsewhere,
and leaves the old referent alone:

<!-- {"fixture":"ref_to_ref_rebinds_the_binding.wado"} -->

```wado
fn retarget(rr: &mut &i32, to: &i32) {
    *rr = to;
}

test "a call rebinds the reference it is handed" {
    let x = builtin::black_box(1);
    let y = builtin::black_box(2);
    let mut r = &x;
    retarget(&mut r, &y);
    assert *r == 2;
    assert x == 1;
}
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

### Writable References

A `&mut T` is [affine](./spec-affine.md): it moves and is never copied, and so
is every type holding one. No affine value
[moves out of a reference](./spec-affine.md#no-move-out-of-a-reference), so a
`&mut T` is not read out of a `&&mut T`, nor is a struct holding one read out
of a `&`. With the rule that
[a `&T` grants reading alone](./spec-expressions.md#variable-mutability),
nothing a `&T` reaches is written through it.

That promise ends at a [shared cell](#shared-cells) and at a closure. A closure
may write a variable it captured or a cell it holds, whichever of `fn` and
`fn mut` it is, and it stays copyable. A closure that captures a `&mut`
[refers into an affine local](./spec-affine.md#references-into-affine-values),
so it does not outlive that local's scope.

Where the expected type is `&mut T`, a `&mut T` reached through a writable
place is reborrowed: the position takes a second reference to the same
referent, and the original stays usable. So a `&mut` parameter is handed to two
calls in a row. Only an expected type reborrows. `let b = a;` moves `a`, and
`let b: &mut T = a;` reborrows it, as in Rust.

A type parameter takes a `&mut F` only where it is declared `affine`
([Generic Code](./spec-affine.md#generic-code)).

> Not yet implemented: a `&mut T` is copied as a `&T` is, and a struct holding
> one is copyable. So a `&mut T` copied out of a `&&mut T`, or held by a struct
> copied out of a `&`, writes its referent.

Rationale: [WEP: Writable References](./wep-2026-11-27-writable-references.md).

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
let mut p = Point { x: 10, y: 0 };
let r1 = &mut p;
let r2 = &mut p;  // OK in Wado (no borrow checker)

r1.x = 20;
r2.x = 30;
assert p.x == 30;
```

A place may have any number of writers. The one limit is on two
[scoped `&mut`](#mut-lasts-for-a-scope) of one place in one call.

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
[`#[storage(...)]`](./spec-attributes.md#storage).

Rationale: [WEP: Value Semantics and Reference Retention](./wep-2026-01-12-value-semantics-and-retention.md).

### References to a Value Replaced on Assignment

A primitive, `enum`, `flags`, copyable `variant`, `fn`, `&` reference or
[shared cell](#shared-cells) is replaced on assignment: it has no storage a reference could share, and it is
not [`RefMut`](./spec-standard-traits.md#dispatch). A `struct`, a tuple, a
`List` and a `String` are written in place, and a reference to one shares its
storage.

An affine value is never copied, a `&mut` and an affine `variant` included, so
a reference to one names its place. A field, tuple element, payload or element
holding an affine variant is a place of its own, so `&mut` of it is the slot
itself. Replacing the value through the reference lands in the slot, and
`opt.take()` on `h.slot` or on `list[i]` leaves `None` there.

#### `&` Is the Value

A `&P` of a type replaced on assignment is a copy of the value, taken where the
`&` is written. That holds for a local, a field and an element alike. An
assignment to the place afterwards does not reach the reference, and for a
`variant` neither does a write to its payload. The lint
[`stale_reference`](./spec-diagnostics.md#the-stale_reference-lint) warns about
a read the assignment left behind.

A generic body that holds a `&T` across an assignment to its place therefore
answers by the kind of `T`: it reads the new value of a `struct` and the old
value of an `i32`.

A closure's capture is not a `&P`. A closure captures the variable, and reads
what the variable holds when it runs
([Capture](./spec-functions.md#capture)).

> Not yet implemented: a `&P` of a local sees a later assignment to the local.
> A `&` of a `variant`, wherever it is taken, shares its payload with the
> place, so it sees a later write to the payload.

#### `&mut` Lasts for a Scope

A `&mut P` of a type replaced on assignment is taken only where the syntax
fixes the point its value goes back to the place:

- A call argument or a receiver: `f(&mut place)`, `place.take()`. The value is
  copied in before the call and copied out after it.
- The scrutinee of a pattern: `if let Some(p) = &mut place { … }`. A binding of
  such a type holds the value for the arm, and the arm's end rebuilds what was
  matched around it and stores that back
  ([Match Ergonomics](./spec-patterns.md#match-ergonomics)).
- A `for let v of &mut xs`, where one turn of the loop is the scope
  ([Semantics](./spec-control-flow.md#semantics) says which operands).

Binding one to a variable, storing one in a value and returning one are compile
errors. A destructuring `let` and a `let … else` bind variables with no arm to
end, so such a binding under a `&mut` scrutinee is an error too. State that outlives a scope is a [shared cell](#shared-cells). The rule
is by type alone: a local, a field and an element take the same `&mut`.

A generic body that only uses a `p: &mut T` takes every `T`. One that stores or
returns a `&mut T` needs `T: RefMut`. A `&mut self` method called on a reference
field reaches the referent, so it takes no `&mut` of the field.

A scope stores back at every exit, whether or not its binding was written: its
end, a `return`, a `break`, a `continue` and a `?`. A trap is no exit: it
stores nothing back, as it runs no [drop](./spec-affine.md#drop). So a write to
the place by another route inside the scope is overwritten, and a read of the
place there sees the value from before the binding was written.

Where the loss is certain it is a compile error:

- An assignment inside the scope, in the same body, to the scope's place, to a
  place holding it or to a part of it.
- A second scoped `&mut` of the same place in one call: `f(&mut x, &mut x)` has
  two stores to one place and no order between them.
- A call of a `&mut self` method on `xs`, or an assignment to it, inside
  `for let v of &mut xs`. It may change the length, and the turn's store would
  land on another element.

A write the compiler cannot place warns under the lint
[`lost_write`](./spec-diagnostics.md#the-lost_write-lint), and the read warns
under `stale_reference`.

> Not yet implemented: a `&mut` of a local of such a type is an ordinary
> reference, which a variable holds and a function returns. A `&mut` of a
> field, or of a `variant` element, that a variable holds or a callee keeps is
> an error only where something replaces the value through it. A pattern
> binding of such a type is an error, a `variant`'s whole binding where it is
> replaced. None of the errors in the list above is reported.

Rationale: [WEP: Writable References](./wep-2026-11-27-writable-references.md).

### Reference Identity

`==` and `!=` on two references compare the values they point to, as in Rust
(see [Eq](./spec-standard-traits.md#eq---equality)). The prelude function
`ref_eq(a: &T, b: &T) -> bool` compares identity instead: whether the two
references point to one place. A place is where a value is stored: a variable,
a field, an element.

Identity is guaranteed in one direction only. Two references to one place are
always `ref_eq` where its type is written in place or is affine: `&x` taken
twice of one variable, or a reference and a copy of it. References to distinct
places may also be `ref_eq`, because the copies value
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

A `&` of a type replaced on assignment
[is a copy of the value](#-is-the-value), so two such references are two
places:

<!-- {"fixture": "spec_memory_ref_identity.wado"} -->

```wado
let r = &xs[0];
xs[0] = 9;
assert *r == 1;
ref_eq(&xs[0], &xs[0]);         // false or true: each `&` takes its own copy
```

An affine variant is the exception: its slot is a place, as
[the previous section](#references-to-a-value-replaced-on-assignment) states.

> Not yet implemented: two `&` of one local are always `ref_eq`, whatever its
> type.

A closure's identity stays unobservable. `ref_eq` takes references only, and a
`&` of a closure is a copy of it, as of any value replaced on assignment:

<!-- {"fixture": "spec_memory_ref_identity.wado"} -->

```wado
let f = || 1;
let g = f;
ref_eq(&f, &f);                 // false or true: each `&` takes its own copy
ref_eq(&f, &g);                 // false or true: two places holding one closure
assert f() == g();
```

## Shared Cells

A shared cell is a reference that is one object under every copy, and that a
`&` may write. Its type is `Shared<T>`, a prelude type the compiler knows.
`Shared::new(value)` puts a value into a new cell, and nothing else makes one:
no cell is taken from a variable or a field, so no cell aliases one.

`*c` is the place the cell holds. It reads and writes whatever path reached
`c`, a `&` included, and `c` needs no `mut`: the dereference of a cell starts
[the path a write goes through](./spec-expressions.md#variable-mutability)
again. A field access, a method call, an index and a `for` loop dereference a
cell as they do a `&mut`, so `for let x of c` is `for let x of &mut *c`. A
cell passes where a `&T` is expected, as a `&mut T` does, and only a cell
where a cell is.

A copy of a cell is the same cell, so the deep copy of a value stops at one.
`Shared::new(*c)` makes a separate cell. `T` is not affine: an affine value has
one owner, which drops it, and a cell every copy shares has none.

<!-- {"fixture": "shared_cell.wado"} -->

```wado
test "a copy of a cell is the same cell" {
    let a = Shared::new(1);
    let b = a;
    *b = 5;
    assert *a == 5;

    let counter = Counter { hits: a };
    let copy = counter;
    *copy.hits = 9;
    assert *counter.hits == 9;
    assert *a == 9;

    let separate = Shared::new(*a);
    *separate = 0;
    assert *a == 9;
}
```

A cast makes no cell and unwraps none, so a cell never names a variable's
storage and no `&mut` outlives the rules of one by coming out of a cell.

What a cell implements follows from its being a reference whose copies are one
object:

| Trait                       | A `Shared<T>`                                             |
| --------------------------- | --------------------------------------------------------- |
| `Eq`                        | where `T` is: `==` compares what the cells hold           |
| `Ord`                       | not implemented, as for a reference                       |
| `Hash`                      | not implemented, as for a reference                       |
| `Default`                   | where `T` is, as `Shared::new(T::default())`              |
| `Inspect`                   | `${c:?}` writes what the cell holds, as `Shared(…)`       |
| `Serialize` / `Deserialize` | not implemented: two holders of one cell would become two |

`ref_eq(a, b)` asks whether two cells are one. A struct holding a cell derives
`Eq` over what its cells hold, and derives none of `Ord`, `Hash`, `Serialize`
and `Deserialize`. A type that wants one of them writes it. A write to a cell
changes a hash written over what it holds, so a key holding a cell is not
written while a map holds it: a lookup would then miss an entry the map still
has.

A cell does not cross a component boundary, for the reason it is not
serialized.

Rationale: [WEP: Writable References](./wep-2026-11-27-writable-references.md).

## Parameters

### Method Receiver: `self` by Value

A method receiver is `&self`, `&mut self` or `self`:

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

A by-value `self` is passed as any parameter is: copied, or [moved](./spec-affine.md#moves) when it is affine. `Option<i32>::unwrap` leaves its binding usable, and so does an iterator adapter: `it.take(2)` takes a copy of `it` and leaves `it` where it was. `(&mut it).take(2)` advances `it` itself, since a `&mut` to an iterator [is an iterator](./spec-standard-traits.md#iterator---core-iteration-trait).

Every value of `Point` is copyable, so a by-value `self` on it copies the receiver at every call. The declaration warns ([`self_by_value`](./spec-diagnostics.md#the-self_by_value-lint)):

<!-- {"fixture":"spec_memory_self_by_value.wado"} -->

```wado
impl Point {
    fn consume(self) -> i32 { return self.x; }
}

export fn run() {
    let p = Point { x: 1, y: 2 };
    assert p.consume() == 1;
    assert p.y == 2;
}
```

Called on a `&` to a copyable value, a by-value `self` takes a copy of what the reference points to:

<!-- {"fixture":"self_by_value_through_reference_copies.wado"} -->

```wado
fn head(r: &Option<i32>) -> i32 {
    return r.unwrap();
}

test "the receiver is copied out of a shared reference" {
    let o = Option::Some(1);
    assert head(&o) == 1;
    assert o matches { Some(1) };
}
```

Where a trait is implemented for the reference itself, the method called on a reference is that impl's, and its `self` is the reference. A blanket impl over a reference (`impl<T: Bound> Tr for &mut T`) answers a by-value `self` this way and leaves `&self` and `&mut self` methods to the referent.

Otherwise a by-value `self` is not called on a `&mut`, nor on a reference to one at any depth (`&&mut T`): the call would act on a copy, where `&mut` says the caller meant the value itself. That is a compile error (`RECEIVER_MISMATCH`), and a written `*` asks for the copy.

<!-- {"fixture":"self_by_value_through_dereference.wado"} -->

```wado
fn or_zero(o: &mut Option<i32>) -> i32 {
    return (*o).unwrap_or(0);
}

test "a dereferenced receiver is copied" {
    let mut o = Option::Some(1);
    assert or_zero(&mut o) == 1;
    assert o matches { Some(1) };
}
```

An affine value is not read out of either reference: the call would [move it out](./spec-affine.md#no-move-out-of-a-reference).

Every impl of a trait's method takes the receiver the trait declares.

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

Writing a `mut` parameter changes only the callee's copy ([Value Semantics](#value-semantics)). A `&mut T` parameter holds the reference, so a write through it (`*p = v`) reaches the referent.

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

## Linear Memory

Beside the GC heap, a program has linear memory: bytes it reaches by address. The `builtin` functions below read and write it, and are as raw as the
Wasm instructions they lower to. An address outside the memory traps. Nothing
else is checked, so which bytes an address names is the caller's to keep right.

### `AnyBits`

`AnyBits` is a sealed marker trait in the prelude. It holds for a type of which
every bit pattern of its width is a value, so a load from any bytes reads one.
The compiler states it, and a user `impl AnyBits` is a compile error.

- It holds for `i8` to `i64`, `u8` to `u64`, `f16`, `bf16`, `f32`, `f64` and
  `v128`.
- It holds for a newtype over a type it holds for, the `core:simd` lane types
  among them.
- It holds for a `flags` type, whose bits are a `u32` that `as` converts either
  way.
- It holds for nothing else. `bool`, `char` and an `enum` leave patterns out. A
  struct, a variant, a tuple or a reference may hold a GC reference. `i128` and
  `u128` are GC objects, and `()` has no width.

### Loads and Stores

| Built-in                                             | Does                                       |
| ---------------------------------------------------- | ------------------------------------------ |
| `builtin::load<T: AnyBits>(addr: i32) -> T`          | reads a `T` at `addr`                      |
| `builtin::store<T: AnyBits>(addr: i32, value: T)`    | writes `value` at `addr`                   |
| `builtin::size_of<T: AnyBits>() -> i32`              | the bytes a `T` takes, as a constant       |
| `builtin::memory_copy(dst: i32, src: i32, len: i32)` | copies `len` bytes; the ranges may overlap |

A load or a store moves exactly `size_of::<T>()` bytes, little-endian, at an
address of any alignment. A load of a signed integer narrower than 32 bits
sign-extends it, and of an unsigned one zero-extends it. A newtype moves as its
base does:

<!-- {"fixture":"spec_memory_any_bits.wado"} -->

```wado
let p = builtin::realloc(0, 0, 16, 32);
builtin::store(p, -1 as i16);
assert builtin::load::<u16>(p) == 0xFFFF;
assert builtin::load::<u8>(p + 1) == 0xFF;

builtin::store(p + 4, 21.5 as Celsius);
builtin::memory_copy(p + 8, p + 4, builtin::size_of::<Celsius>());
assert builtin::load::<Celsius>(p + 8) == 21.5 as Celsius;
assert builtin::load::<u32>(p + 8) == 0x41AC0000;
```

A type argument that is not `AnyBits` is a compile error:

<!-- {"fixture":"builtin_any_bits_refused.wado"} -->

```wado
let b = builtin::load::<bool>(p);
let c = builtin::load::<char>(p);
```

### Memory64

A program also has a second linear memory, of 64-bit addresses, which the
canonical ABI never reads. It holds what outgrows the first one's 4 GiB, and
its built-ins take `i64` addresses:

| Built-in                                                    | Does                                     |
| ----------------------------------------------------------- | ---------------------------------------- |
| `builtin::memory64_load<T: AnyBits>(addr: i64) -> T`        | reads a `T` at `addr`                    |
| `builtin::memory64_store<T: AnyBits>(addr: i64, value: T)`  | writes `value` at `addr`                 |
| `builtin::memory64_size() -> i64`                           | its size, in 64 KiB pages                |
| `builtin::memory64_grow(delta: i64) -> i64`                 | grows it by `delta` pages; the old size  |
| `builtin::memory64_fill(dst: i64, value: u8, len: i64)`     | sets `len` bytes to `value`              |
| `builtin::memory64_copy(dst: i64, src: i64, len: i64)`      | copies within it; the ranges may overlap |
| `builtin::copy_to_memory64(dst: i64, src: i32, len: i32)`   | copies `len` bytes from the first memory |
| `builtin::copy_from_memory64(dst: i32, src: i64, len: i32)` | copies `len` bytes to the first memory   |

It starts empty. The two memories are apart: an address names different bytes
in each.
