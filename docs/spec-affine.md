# Affine Types

An affine type is move-only: a value of it is used by value at most once. An
affine resource is one, and so is every type that holds one at any depth. The
rest of this chapter follows from four rules:

- A position that takes a value moves it. Every other position reads the value
  where it stands.
- A value has one owner. A reference owns nothing, so no value moves out
  through one.
- A value that has not moved drops exactly once, when its owner stops holding
  it.
- Code that may copy a value is not handed an affine one.

Rationale: [WEP: Affine Types](./wep-2026-10-17-affine-types.md).

## Declarations

A named `struct` or `variant` that holds an affine value in a field or a case
is declared `affine`. One that holds none may not be. Either mismatch is an
error naming the declaration (`AFFINE_DECLARATION`). A field typed by a type
parameter holds nothing for this rule, since the parameter is copyable unless
an instantiation says otherwise. A tuple, an anonymous struct and a newtype have
no declaration to mark, so each is affine when something it holds is.

<!-- {"fixture":"affine_declarations.wado"} -->

```wado
affine struct Holder {
    c: Counter,
    n: i32,
}

affine variant Slot {
    Empty,
    Held(Holder),
}

affine struct Bag {
    items: List<Counter>,
}

type Wrapped = Holder;

test {
    let slot: Slot = Empty;
    assert slot matches { Empty };
    let bag = Bag { items: [] };
    assert bag.items.is_empty();
}
```

## Moves

The positions that take a value are a binding, a by-value parameter, a
`return`, an element of an aggregate, a by-value `self`, an `as` cast, a
by-value pattern, and the handler of a `with`. Using a place after a move out
of it is a compile error (`TYPE_MISMATCH`).

<!-- {"fixture":"spec_components_use_after_move.wado"} -->

```wado
pub resource Counter {
    fn bump(&self);
    fn consume(self);
}

fn eat(c: Counter) { }

fn misuse(c: Counter, d: Counter) {
    eat(c);          // moves `c`
    c.bump();        // ERROR: affine value `c` used after it was moved
    d.consume();     // a by-value `self` moves `d` too
}
```

The check follows control flow, and tracks each place: a binding, or a field
path under it. Moving `h.f` out of an owned `h` leaves its other fields usable.
Storing into a place makes it usable again.

A closure body and a `match` guard see what they reach by reference, so neither
moves it. A global is never read by value, since no check follows it across
functions.

A `match` or `if let` consumes its scrutinee as an arm is entered. A refutable
pattern that fails consumes nothing, so the `else` of an `if let` or a
`let ... else` still holds the scrutinee. A `matches` consumes nothing, whether
it matches or not.

### No Move Out of a Reference

Reading an affine value by value through a `&` or `&mut`, `&self` included, is
a compile error. Indexing reads an element through a reference, so `list[i]`
is such a read too.

<!-- {"fixture":"spec_components_move_out_of_reference.wado"} -->

```wado
affine struct Holder { f: Fields }

impl Holder {
    fn peek(&self) -> Fields {
        return self.f;       // ERROR: cannot move affine type `Fields` out of a reference
    }
}
```

<!-- {"fixture":"spec_components_consume_holder.wado"} -->

```wado
impl Holder {
    fn into_fields(self) -> Fields {
        return self.f;       // OK: the holder is consumed
    }
}

test {
    let holder = Holder { f: Fields::new() };
    assert holder.into_fields().copy_all().is_empty();
}
```

## Generic Code

A type parameter written `affine T` admits an affine type argument, and its
body is checked with `T` affine. An associated type written `affine type Item;`
is affine the same way.

<!-- {"fixture":"affine_param_moves_once.wado"} -->

```wado
fn wrap<affine T>(x: T) -> Option<T> {
    return Option::Some(x);
}

test {
    let f = wrap(Fields::new()).unwrap();
    assert f.copy_all().is_empty();
}
```

A type parameter or an associated type without the mark may be copied. An
affine argument for one, or an `impl` binding an affine type to one, is an error
naming it (`AFFINE_ARGUMENT`).

<!-- {"fixture":"affine_argument_for_unmarked_param.wado"} -->

```wado
fn dup<T>(x: T) -> [T, T] {
    return [x, x];
}

export fn run() {
    let pair = dup(Fields { n: 1 });
}
```

The mark belongs to the parameter's declaration, not its name: a method's `T`
that shadows its impl's `affine T` is unmarked unless written so. An `impl`
method restates every `affine` its trait's method declares, since a call
through the trait may hand it an affine argument.

An `impl` whose type parameter is unmarked does not apply to an affine
argument, so its methods are not found on that instance.

<!-- {"fixture":"affine_argument_misses_unmarked_impl.wado"} -->

```wado
struct Cell<affine T> {
    value: T,
}

impl<T> Cell<T> {
    fn twice(&self) -> [T, T] {
        return [self.value, self.value];
    }
}

export fn run() {
    let cell = Cell { value: Fields { n: 1 } };
    let pair = cell.twice();
}
```

An affine type implements only an `affine trait`, whose default bodies and
signatures treat `Self` as affine. Implementing a trait without the mark for an
affine type is an error at the impl (`AFFINE_DECLARATION`).

<!-- {"fixture":"affine_impl_of_affine_trait.wado"} -->

```wado
affine trait Named {
    fn name(&self) -> String;

    fn shout(&self) -> String {
        return `${self.name()}!`;
    }
}

impl Named for Holder {
    fn name(&self) -> String {
        return "holder";
    }
}

test {
    let h = Holder { f: Fields::new() };
    assert h.shout() == "holder!";
}
```

## Drop

The owners of a value are a parameter, a local, a temporary and an aggregate.
An owner stops holding a value at the first of these:

- Its scope ends, on any path out of it. A function's parameters are the
  outermost locals of its body.
- It is a temporary or a value its statement discards (`make();`,
  `let _ = make();`), and its statement ends. A `with` owns a handler it is
  given by value the same way.
- An assignment replaces it: `x = value` drops what `x` held, and a store into a
  field or an element drops what the slot held.
- It is the part of a by-value pattern that no binding takes. It drops as the
  arm is entered, after the guard, or as the `let` binds.
- What holds it drops.

The owners that stop together drop in the reverse of the order they began
holding. A trap does not unwind, so it runs no drops.

<!-- {"fixture":"affine_pattern_drops_unbound_parts_on_arm_entry.wado"} -->

```wado
test "a struct pattern drops the fields it leaves out" {
    let d = DROPS;
    let p = P { a: Tok { id: 1 }, b: Tok { id: 2 } };
    match p {
        P { a, .. } => {
            assert DROPS - d == 1;
            assert eat(a) == 1;
            assert DROPS - d == 2;
        },
    }
    assert DROPS - d == 2;
}
```

What a drop does is the type's `Drop`, a prelude trait:

```text
pub affine trait Drop with () {
    fn drop(self);
}
```

- A type with an impl runs its `drop`. The body may move fields out of `self`,
  and the fields left at its end are dropped. A pattern may take `self` apart,
  but nothing may hold it whole, since wherever it went it would run `drop`
  again.
- A type without an impl drops its fields in declaration order.
- Only an affine type implements `Drop`, since a copyable type would run it for
  every copy. A type that implements it is never partially moved, outside its
  own `drop`: no field read, pattern or
  [`..base`](./spec-literals.md#base-spread) takes an affine part of it.
- `builtin::drop(value)` drops any value where it is called.

A drop's effects are benign: a function declares no effect for a drop it
performs, written or implicit. A `drop` that performs an effect other than its
own resource's names it with [`#[benign(E)]`](./spec-attributes.md#benigne-).

## References into Affine Values

A value refers into a local when it is a reference to the local or to a place
inside it, a closure capturing it, or a value built from one whose type may hold
a reference: a struct or tuple holding one, or what a call returns when it was
handed one. A call may store what any of its operands refers into through each
`&mut` it takes.

A borrow `&x` or `&mut x` handed to a parameter declared `&P` or `&mut P` stays
out of the result where the declared return type holds no reference that could
point into a `P`: none to `P` itself, to a type a `P` holds, or to a type
parameter. It is likewise never stored through another parameter declared
`&mut Q` where `Q` holds no such reference. Such a result, or such a `Q`, still
carries what `x` refers into. A parameter declared a type parameter, or a callee
that is a closure or a `fn` value, hands the borrow on.

<!-- {"fixture":"affine_reference_call_result_unrelated.wado"} -->

```wado
test {
    let mut t = Tok { id: 1 };
    let n = Name { text: "a" };
    let id = read(&mut t);
    let v = view(&t, &n);
    let w = t.label(&n);
    let x = Tok::label(&t, &n);
    let moved = t;
    assert id == 1;
    assert v.name.text == "a";
    assert w.name.text == "a";
    assert x.name.text == "a";
    assert moved.id == 1;
}
```

A value referring into an affine local is read only while the local holds what
it read. The compiler refuses (`TYPE_MISMATCH`):

- a read of it after the local moved, or after the local's scope ended;
- a read of it after a `&mut` use of the local other than through it, such as a
  method taking `&mut self`, which may have moved out a part of the local;
- a `return` of it from the body declaring the local;
- a `&mut` parameter, or a closure's capture, left holding it when the body
  ends;
- a store of it into a global.

<!-- {"fixture":"affine_reference_used_after_move.wado"} -->

```wado
test {
    let t = Tok { id: 7 };
    let r = &t;
    builtin::drop(t);
    assert r.id == 7;
}
```
