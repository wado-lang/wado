# Patterns

A pattern tests a value's shape and binds names to its parts. The same pattern
syntax appears wherever a value is taken apart: `match` arms, `if let`,
`while let`, `let … else`, `matches`, `let` and `for-of` bindings, and
parameters. The statements themselves are in [Control Flow](./spec-control-flow.md).

## Pattern Syntax

| Pattern       | Example                      | Description                                  |
| ------------- | ---------------------------- | -------------------------------------------- |
| Wildcard      | `_`                          | Matches anything                             |
| Variable      | `x`                          | Binds matched value                          |
| Mut variable  | `mut x`, `Some(mut x)`       | Binds as mutable                             |
| Literal       | `0`, `"hello"`, `true`       | Matches exact value                          |
| Constant      | `MAX_LEN`, `i32::MAX`        | Matches an immutable global / const by value |
| Range         | `0..<60`, `'a'..='z'`        | Matches an integer, `char` or float in range |
| Variant       | `Some(x)`, `None`            | Matches variant case                         |
| Tuple         | `[a, b, c]`                  | Destructures tuple                           |
| Nested tuple  | `[10, Some(x)]`              | Literal/variant sub-patterns in tuple        |
| Struct        | `{ x, y }`, `Point { x, y }` | Destructures struct                          |
| Nested struct | `{ x: 0, y }`                | Literal/variant sub-patterns in struct       |
| Or            | `Red \| Blue`                | Matches either pattern                       |
| Guard         | `Some(x) && x > 0`           | Pattern with condition                       |
| Type          | `input: HtmlInputElement`    | Matches when the value is of the type        |

A tuple pattern names every element of the tuple, or ends in `..` after at most
that many. Over a tuple carrying a variadic pack, only the elements ahead of the
pack have a fixed position, so the pattern names at most those and ends in `..`.

A string-literal pattern tests the scrutinee with `==` against a `String`, so
any type implementing `Eq<String>` matches one: a `String`, a `StrSlice`, or a
newtype over either. A type parameter bounded by
[`AsStrSlice`](./spec-standard-traits.md#asstrslice) matches as well.

A float literal is not a pattern. A float matches a
[range pattern](#range-patterns) instead.

A literal or range pattern on a value whose type is a type parameter means, in
each instance, what it means on that instance's type. An instance whose type
the literal is not, or cannot hold its value, is an error at the pattern.

<!-- {"fixture":"match_literal_pattern_type_param.wado"} -->

```wado
fn is_minus_one<T>(x: T) -> i32 {
    return match x {
        -1 => 1,
        _ => 0,
    };
}

test "a negative literal matches each signed width" {
    assert is_minus_one(builtin::black_box(-1_i8)) == 1;
    assert is_minus_one(builtin::black_box(-1_i64)) == 1;
    assert is_minus_one(builtin::black_box(1_i32)) == 0;
}
```

<!-- {"fixture":"spec_control_flow_match.wado"} -->

```wado
fn kind<S: AsStrSlice>(s: S) -> i32 {
    return match s { "cntrl" => 1, "digit" => 2, _ => 0 };
}

test {
    assert kind("digit".as_str_slice()) == 2;
}
```

## Patterns That Cannot Fail

A `let` or `for` binding takes a pattern that matches every value of its type.
A case pattern does so when no other case of the type holds a value
([Exhaustiveness](#exhaustiveness)). A literal, a range, a constant, an or-pattern, and a
narrowing type pattern never do.

<!-- {"fixture":"spec_control_flow_match.wado"} -->

```wado
variant W { A(i32) }

test {
    let w = W::A(1);
    let r: Result<i32, !> = Ok(2);
    let A(x) = w;                     // OK: W has no other case
    let Ok(v) = r;                    // OK: r has no `Err`
    assert x == 1 && v == 2;
}
```

A case pattern of a type with another case is an error, since that case may be
the value:

<!-- {"fixture":"spec_control_flow_let_refutable.wado"} -->

```wado
let opt: Option<i32> = Option::Some(1);
let Some(y) = opt;
```

A bare name in such a pattern, at its root or below it, is a case pattern when
it names a case of the type it matches, and a binding otherwise. It never names
a global: a constant pattern can always fail, so reading one here could only be
rejected. So `let [None, n] = pair` tests its first element, and is an error
since `None` may not match. `let limit = 1` binds even where a `global limit` is
in scope, and [`shadowed_name`](./spec-expressions.md#the-shadowed_name-lint)
warns. A refutable pattern reads such a name differently
([Constant Patterns](#constant-patterns)).

An uninitialized `let x: T;` declares a single name, or `_`.

## Exhaustiveness

A `match` must cover every value of its scrutinee. A `_` arm covers whatever the
other arms leave:

<!-- {"fixture":"spec_control_flow_match.wado"} -->

```wado
let name = match color {
    Red => "red",
    Green => "green",
    _ => "other",  // Required for exhaustiveness
};
assert name == "other";
```

The guardless arms must cover every value together. A guarded arm covers
nothing, and a case is covered only as far as its payload patterns reach. This
match misses `Some(-2147483648..=0)`, among others:

<!-- {"fixture":"spec_control_flow_non_exhaustive.wado"} -->

```wado
let s = match opt {
    Some(1) => "one",
    None => "none",
};
```

An arm no value can reach is an error too: the guardless arms before it already
take every value it matches.

<!-- {"fixture":"spec_control_flow_unreachable_arm.wado"} -->

```wado
let n = match opt {
    _ => 0,
    Some(x) => x,
};
```

A case whose payload type has no value needs no arm, since nothing constructs
it. An arm naming one is unreachable, and is the same error. A type has no value
when it is `!`, when a tuple element or struct field of it has none, or when
every case of a variant has a payload with none:

<!-- {"fixture":"spec_control_flow_match.wado"} -->

```wado
let r: Result<i32, !> = Ok(1);
let n = match r {
    Ok(v) => v,     // exhaustive: no `Result<i32, !>` is an `Err`
};
assert n == 1;
```

## Guard Expressions

A guard follows the pattern after `&&`, since it runs after the pattern
matches, and only then:

<!-- {"fixture":"spec_control_flow_match.wado"} -->

```wado
let discount = match customer {
    Premium(years) && years > 5 => 0.3,
    Premium(_) => 0.2,
    _ => 0.1,
};
assert discount == 0.3;
```

## Qualified Patterns

A case may be written under the type that declares it: `Color::Green`. Two kinds
of qualifier reach the same cases.

The first is a name that resolves to the scrutinee's type. An import alias
(`M::Nothing` under `use { Maybe as M }`), `Self` inside an `impl`, and a
namespace-qualified type (`dep::Maybe::Nothing`) all qualify. So does any name on
the scrutinee's newtype chain, a newtype's cases being its base's: with
`type C = Color`, both `C::Green` and `Color::Green` qualify. A second newtype
over the same base does not, being a distinct type.

The second is a namespace prefix the scrutinee's type is reachable through, such
as `h::Green` under `use h from "./hue.wado"`. That prefix names a module rather
than a type.

Either way the name must be one the file can see. Only the prelude is in scope
without a `use`, so a qualifier naming an unimported type is an error even where
the bare case would match.

A qualifier may restate the scrutinee's type arguments. It must then write as
many as the scrutinee carries, so `Maybe<i32>::Just` qualifies a `Maybe<i32>`
while `Color<i32>::Red` is an error, because `Color` declares no type parameters.

Only a bare identifier can bind. A qualified path names a case, an associated
constant, or an immutable `global`; anything else is an error, never a variable
of that name.

## Constant Patterns

A pattern identifier that resolves to an immutable `global` or an associated constant matches by value, instead of binding a new variable:

<!-- {"fixture":"spec_control_flow_match.wado"} -->

```wado
global TK_FOO: i32 = 1;
global TK_BAR: i32 = 2;

fn token_kind(token: i32) -> String {
    return match token {
        TK_FOO | TK_BAR => "keyword",
        i32::MAX        => "max",
        _               => "other",
    };
}

test {
    assert token_kind(2) == "keyword" && token_kind(i32::MAX) == "max";
    assert token_kind(3) == "other";
}
```

The pattern matches where `scrutinee == CONSTANT` holds, so a constant of any
type with an `Eq` compares as `==` would: a `String`, a struct, a tuple or an
`Option` constant matches at the top of an arm or nested in another pattern.

A constant is not a pattern when its type is a float or holds one, in a field,
element or payload at any depth. So `f64::INFINITY`, a `global` holding `0.1`,
and a struct constant with an `f64` field are errors here. A float constant is
still a [range bound](#range-patterns).

Only a refutable pattern reads a bare name as a constant: a `match` arm,
`if let`, `while let`, and `let ... else`. So `let limit = v else { … }` runs
the `else` block unless `v == limit`. A local, a parameter, or a closure capture
that takes the name puts the global out of reach, so the name binds there as it
would anywhere else. What counts is what is in scope where the pattern starts.
A name the pattern binds itself reaches none of its own sites, so
`[mut limit, limit]` binds the first element and tests the second against the
global.

## Range Patterns

A range pattern matches a value between its bounds. It stands wherever a
refutable pattern may, nested ones included:

<!-- {"fixture":"spec_control_flow_match.wado"} -->

```wado
let grade = match score {
    0..<60 => "F",
    60..=100 => "P",
    _ => "invalid",
};
let lower = c matches { 'a'..='z' };
assert grade == "P" && lower;
```

- The scrutinee is an integer, `char` or float.
- Each bound is an integer, `char`, byte or float literal, optionally negated,
  or a [constant](#constant-patterns): an immutable `global`, bare or under a
  namespace, or an associated constant.
- A literal bound takes the scrutinee's type as a literal does where that type
  is expected, so on a float an integer or byte literal is the float of its
  value (`0..<1.5`). A suffixed literal and a constant keep their own type.

The compiler knows the value of a literal and of a primitive type's limit, such
as `i32::MAX` or `f64::INFINITY`. A range bounded by those alone is checked
before the program runs:

- A reversed range pattern is an error, and so is an empty one (`5..<5`).
- Two arms' range patterns must not overlap. The alternatives of one arm's
  or-pattern may.
- Range patterns count toward [exhaustiveness](#exhaustiveness): `0 => …` and
  `1..=255 => …` together cover a `u8`. A `char` holds no surrogate, so
  `'\0'..='\u{D7FF}'` and `'\u{E000}'..='\u{10FFFF}'` together cover it.

Any other constant shows its value only when the match runs. A range bounded by
one matches where `START <= scrutinee` and `scrutinee < END` (`<=` for `..=`)
hold by the type's order, as a constant pattern matches where `==` holds. It
covers no value for exhaustiveness:

<!-- {"fixture":"range_pattern_constant_bounds.wado"} -->

```wado
let score = builtin::black_box(15);
let tier = match score {
    LOW..<HIGH => 1,
    HIGH..=Grade::PASS => 2,
    _ => 0,
};
assert tier == 1;
```

A float range compares by the
[float order](./spec-standard-traits.md#float-comparison), so `0.0..<1.0`
matches `-0.0`:

<!-- {"fixture":"float_range_pattern.wado"} -->

```wado
let zero = builtin::black_box(-0.0);
assert zero matches { 0.0..<1.0 };
let nan = builtin::black_box(f64::NAN);
assert !(nan matches { f64::NEG_INFINITY..=f64::INFINITY });
```

Two more rules apply to one the compiler checks:

- A NaN bound is an error. A NaN scrutinee therefore matches no range, and a
  `match` on a float needs a `_` arm. `f64::NEG_INFINITY..=f64::INFINITY`
  matches every float but a NaN.
- A range whose bounds are equal (`1.0..=1.0`) is an error, as the float literal
  it stands for is.

The range operators themselves are in [Ranges](./spec-expressions.md#ranges).

## Or Patterns

Or patterns match if any alternative matches. All alternatives must bind the same names with the same types:

<!-- {"fixture":"spec_control_flow_match.wado"} -->

```wado
// Enum or-patterns
let tone = match color {
    Red | Blue => "cool",
    Green => "warm",
};
assert tone == "cool";

// Variant or-patterns with bindings
let value = match expr {
    Num(n) | Neg(n) => n,
    Zero => 0,
};
assert value == 7;

// Literal or-patterns
let level = match count {
    1 | 2 | 3 => "low",
    _ => "high",
};
assert level == "low";

// Or patterns in matches operator
assert shape matches { Circle(_) | Square(_) };
```

## Nested Sub-Patterns

A tuple or struct pattern may hold any refutable pattern in an element or field:
a literal, a case, an or-pattern, or a range.

<!-- {"fixture":"spec_control_flow_match.wado"} -->

```wado
// Literal sub-patterns in tuples
let axis = match [dx, dy] {
    [0, 0] => "origin",
    [0, _] => "y-axis",
    [_, 0] => "x-axis",
    _ => "other",
};
assert axis == "y-axis";

// Variant sub-patterns in tuples
let sum = match [a, b] {
    [Some(p), Some(q)] => p + q,
    [Some(p), None] => p,
    [None, _] => 0,
};
assert sum == 3;

// Literal sub-patterns in structs
let point = Pt { x: 0, y: 0 };
let mut origin = false;
if let { x: 0, y: 0 } = point { origin = true; }
assert origin;

// Enum sub-patterns in tuples
let label = match [color, size] {
    [Red, Large] => "big red",
    [Blue, _] => "blue",
    _ => "other",
};
assert label == "big red";

// Or-pattern and range sub-patterns
let p = Pt { x: 15, y: 4 };
let v = match p {
    { x: 0 | 1, y } => y,
    { x: 10..=20, y } => -y,
    _ => 0,
};
assert v == -4;
```

## Mutable Bindings in Patterns

`mut` before a binding name makes that binding mutable:

<!-- {"fixture":"spec_control_flow_match.wado"} -->

```wado
if let Some(mut x) = opt {
    x += 10;  // x is mutable
    assert x == 11;
}

let doubled = match result {
    Ok(mut value) => {
        value *= 2;
        value
    },
    Err(_) => 0,
};
assert doubled == 42;
```

## Match Ergonomics

When the scrutinee of `if let`, `match`, or `matches` is a reference (`&T` or
`&mut T`), patterns match against the type it points to, and payload bindings
become references. Matching `&Option<T>` with `Some(x)` gives `x: &T`, not
`x: T`, as Rust's match ergonomics (RFC 2005) do.

Under a `&mut` scrutinee, a payload of a replace-on-assign type (a primitive,
`enum`, `flags` or `fn`) cannot bind at all, even to be read: matching
`&mut Option<i32>` with `Some(x)` is an error. Match `*r` to bind the payload
by value, or a `&` to the value to bind a `&i32`. See
[Mutable References to Fields and Elements](./spec-memory.md#mutable-references-to-fields-and-elements).

A destructuring `let` or `for` binding follows the same rule, and so does a reference met below the top of a pattern: `for let [a, b] of &pairs` gives `a: &A`, and `[n, { x, .. }]` against `[i32, &Point]` gives `x: &i32`.

<!-- {"fixture":"spec_control_flow_conditionals.wado"} -->

```wado
let opt: Option<i32> = Option::<i32>::Some(42);
let ro = &opt;
if let Some(x) = ro {       // ro: &Option<i32>, x: &i32
    assert *x == 42;        // dereference to use the value
}
```

## Type Patterns

A pattern may ascribe a type: `p: T` matches when the subject is a `T`, and `p` binds it. The ascription on a `let` is this pattern, so one rule covers both spellings.

Whether the pattern can fail is decided statically, from the subject's type `S`:

| Relation          | Meaning                                                             |
| ----------------- | ------------------------------------------------------------------- |
| `S <: T`          | irrefutable: an upcast, or an ordinary type annotation              |
| `T <: S`, `T ≠ S` | refutable: a runtime test, and only where `extends` relates the two |
| otherwise         | a type error, as a mismatched annotation is                         |

An irrefutable ascription still drives type context, so `let x: i64 = 42` coerces the literal. A refutable one needs a pattern position that admits failure, so `let` and a `for` binding reject it exactly as they reject `Some(x)`:

<!-- {"fixture":"spec_components_type_patterns.wado"} -->

```wado
fn check(el: Element, node: Node) {
    let n: Node = el;                                   // Element <: Node — irrefutable upcast
    if let input: HtmlInputElement = el { assert input == n; }
    if node matches { _: Element } { assert kind(node) == "element"; }  // the predicate form
    let input: HtmlInputElement = el else { return; };  // the guard form
    assert kind(input) == "input";
}

fn kind(node: Node) -> String {
    return match node {
        input: HtmlInputElement => "input",
        elem: Element => "element",
        _ => "other",                                   // required: the hierarchy is open
    };
}
```

A refutable ascription in a plain `let` is rejected:

<!-- {"fixture":"spec_components_let_refutable.wado"} -->

```wado
let input: HtmlInputElement = el;                   // ERROR: refutable pattern in `let`
```

A type match over resources always needs a final `_` arm, because the host may hand back a type the program does not name. An unguarded arm whose type is a supertype of a later arm's makes that later arm unreachable, which is an error, as [any unreachable arm](#exhaustiveness) is.

A refutable ascription tests a handle, so it binds a name or `_` and nothing deeper, and its subject is the value rather than a reference to it. `T` must be a concrete type: a type parameter says nothing about whether it narrows.
