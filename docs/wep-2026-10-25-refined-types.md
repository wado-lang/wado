# WEP: Refined Types — Types That Carry a Contract

## Context

A value often has a permitted range that its type does not state: a user name
of 1 to 64 characters, an age from 0 to 150, a page size that is a power of two.
Today the range is checked in one of two places, and neither is the type.

- A `contract` clause checks it where a function is called. Every function
  that takes the value restates the clause.
- A `struct` with a private field and a checked constructor holds it for good.
  [Newtype](./spec-types.md#newtype) points there, and so does
  [Contract Checks](./wep-2026-10-06-contract-checks.md#a-contract-states-inputs-only),
  which leaves "a newtype that carries an invariant" to a proposal of its own.
  The wrapper costs a declaration, a constructor and an accessor per type, and
  it hides every method of the value it wraps.

[Jade](./wep-2026-06-13-jade.md) adds a reader. A framework that serves HTTP
from typed handlers, as FastAPI does, describes its request bodies as JSON
Schema. `minLength: 1` has to come from the same place as the check, or the
documented API and the served one drift apart. So the range must be enforced by
the language and readable by a library.

### What was tried first

Three designs were tried while revising Jade. Each one failed in its own way.

- A `Constrained` trait on a newtype. A library reads it, but nothing enforces
  it: `"" as UserName` passes, and Jade cannot implement `Deserialize` for a
  type it does not own.
- A typed `#[metadata(…)]` attribute on a field. A library reads it, but only
  that library enforces it, so it is a convention, not a guarantee.
- A newtype that carries an invariant. Enforcing it took one rule per hole.
  Mutating methods had to be removed, since `name.push_str(…)` breaks a length
  bound. Results had to stop being retyped, since `age + age` would be an
  `Age`. Traits that produce `Self` had to stop being inherited, since
  `Default`, `FromStr` and `collect()` would mint an `Age` unchecked.

The third design patched newtypes until they no longer behaved like newtypes.
The patches had one cause. A newtype is a different type with the same values,
and it inherits by substituting itself for its base. An invariant asks for the
opposite: the same type with fewer values.

### Prior art

- Ada's subtypes are the nearest design. `subtype Age is Integer range 0 .. 150`
  is an `Integer`, its arithmetic is `Integer`'s, and assigning to an `Age`
  checks the range. Ada 2012 adds `Static_Predicate` and `Dynamic_Predicate`
  for any condition. The known hole is that a predicate is not checked when a
  component of a composite object is modified in place.
- Racket uses one word, `contract`, for a function's arguments and for a value
  bound to a name, and blames the side that broke it.
- pydantic writes the range into the type
  (`Annotated[str, StringConstraints(max_length=200)]`), so validation and the
  JSON Schema read the same data. Validation runs at the model boundary, and an
  assignment after construction goes unchecked unless `validate_assignment` is
  turned on, which is a common surprise.
- Liquid Haskell and F\* prove refinements statically. The proof is what makes
  them heavy, and what their users struggle with.
- Manifest contracts (Greenberg, Pierce and Weirich, "Contracts Made Manifest",
  POPL 2010) keep the refinement type `{x:B | e}` and check `e` by a cast at
  run time. Hybrid type checking (Flanagan, 2006) proves what it can and casts
  the rest.
- Scala's `refined` and `iron` and Haskell's `refined` libraries refine at run
  time and return an `Either`, and check a literal at compile time.

## Decision

### A refined type is its base with contract clauses

`B contract C` is the type of the values of `B` for which the clause `C` holds.
It is written wherever a type is, and repeating `contract` adds clauses, all of
which must hold:

```text
type UserName = String contract Length { min: 1, max: 64 };
type Age = i32 contract 0..=150;
type Handle = String contract Length { min: 3, max: 15 } contract self.is_ascii();

struct CreateUser {
    name: UserName,
    bio: String contract Length { max: 200 },
}
```

A refined value is represented as its base is, so the refinement costs nothing
to hold. Only producing one costs a check.

In the type theory literature, "refinement type" usually means one whose
predicate is proven statically. Wado checks the clause at run time, where a
value enters the type, and the type records that the check passed. That is a
manifest contract, the design Scala's and Haskell's `refined` libraries also
ship under this name.

The share decided at compile time grows as the compiler learns to evaluate more
clauses, as in hybrid type checking. A conversion the compiler proves needs no
check at run time, and one it refutes is reported (see
[the explicit conversions](#a-value-enters-a-refined-type-only-by-an-explicit-conversion)).
Either way the conversion stays written: proving more removes checks and
reports more failures, but never lets a narrowing go unwritten.

A refined type is not a newtype. `type UserName = String contract …` declares a
newtype whose base is the refined type `String contract …`. The newtype gives
the name its distinctness, and the refinement gives it its invariant. Neither
construct changes the other.

### `assert` and `contract`

The language has two words for a condition that must hold. They split on who
owes it:

| Word                 | Who owes the condition         | Where it is checked     | `unchecked` |
| -------------------- | ------------------------------ | ----------------------- | ----------- |
| `assert`             | the code that states it        | where it stands         | no          |
| `contract` on a `fn` | the caller                     | at the call             | yes         |
| `contract` on a type | whoever produces a value of it | where a value enters it | yes         |

A function's contract and a type's are one idea at two boundaries, a call and a
value. They share their semantics: when a check runs, whom it blames, how
`-f contract-checks` and `unchecked` treat it, and what a clause may do. A
clause performs no effect, writes only the values it creates, and names nothing
less visible than the declaration it guards.

### A clause is a condition or a value

A clause on a type takes one of two forms, told apart by the type of the clause:

- A `bool` expression over `self`, the value being checked. It is enforced, and
  a library cannot read it. A failed check reports the condition and its
  operands, as `assert` does.
- A value whose type implements `Refine<B>`. It is enforced, and reflection
  hands the value to a library. The value is a constant expression, evaluated
  as a global initializer is.

```text
pub trait Refine<T> with () {
    fn holds(&self, value: &T) -> bool;
    fn describe(&self) -> String;
}
```

A range is such a value already: `RangeInclusive<i32>` implements
`Refine<i32>`, so `i32 contract 0..=150` reads as Ada's `range 0 .. 150`. The
other common clause types, such as `Length`, live in a standard library module
outside the prelude.

A function's contract takes the `bool` form only. It has no subject: a value
clause there would not say which argument it binds. A condition on one argument
can be written either way, and the two differ in who checks it. A `contract`
clause is checked at the call. A refined parameter type is checked where the
caller made the value, so the caller passes one it has already refined:

```text
fn rename(user: &mut User, name: String contract Length { min: 1, max: 64 }) { ... }

fn clamp(v: i32, lo: i32, hi: i32) -> i32
    contract lo <= hi
{ ... }
```

### Widening is free, and `&mut` does not widen

A refined value is a value of its base. Two rules follow, and they are the ones
[Resource Inheritance](./spec-components.md#subtyping) already states for
`resource extends`:

1. `R` converts to `B`, and `&R` to `&B`, implicitly, wherever a `B` is
   expected. `as` may widen, and never narrows.
2. `&mut R` never converts to `&mut B`. A write through the `&mut B` could
   install a value outside `R`.

Everything else follows from these two:

- A method of `B` is called on an `R` by widening the receiver. Its signature is
  `B`'s, so `age + age` is an `i32` and `name.trim()` is what it is on a
  `String`. Nothing is retyped, so nothing mints an `R`.
- A method taking `&mut self` cannot be called on an `R`, since the receiver
  would have to widen as `&mut B`. The diagnostic says so, in the way a
  [declared absence](./wep-2026-09-13-declared-absence.md) does, rather than
  reporting no method of that name. A refined value changes only by being
  replaced whole, and the replacement enters the type through a check.
- A field of a refined struct is read through the widened value, and cannot be
  assigned, for the same reason. The hole Ada leaves, a component modified in
  place, does not exist here.
- A newtype over a refined type inherits by the newtype rules, and its chain
  ends at the refined link. The newtype's `as` reaches the refined type and
  widens past it, but an `as` that would narrow into it is an error. Its
  inherited methods are `B`'s, reached by widening, and their signatures never
  name the refined type, so substitution leaves them returning `B`.

Refined types also make immutability something a program can ask for: a refined
`List` exposes every reading method and no mutating one.

### Generic code sees the refined type, and reads it only

A type parameter is solved to the refined type, the most specific one, as it is
for a resource. Solving it to the base would not avoid the question below:
`List<Age>` fixes its element type, so `ages.sort()` needs `Age: Ord` either
way.

A refined type meets a bound through its base's impl only where the trait reads
a `Self` and nothing more: every `Self` in its methods' signatures is a `self`,
`&self`, `Self` or `&Self` parameter. A trait that produces a `Self`, such as
`Default`, `FromStr`, `From` or `FromIterator`, would mint a refined value
unchecked. A trait with a `&mut self` method would write one in place, which
the `&mut` rule above forbids. Either is met only by an impl for the refined
type itself, which is how the compiler's `Deserialize`, `from` and `try_from`
reach it.

An impl that applies to the refined type itself, a blanket bounded on
`ReflectRefined` included, wins over its base's, as an impl for a newtype wins
over the one it inherits. That is how Jade's schema for a refined type adds its
keywords to the base's.

The base's impl is used as written, with nothing substituted. `Age: Add` has
`i32` as its `Output`, so `fn twice<T: Add<Output = T>>(a: T) -> T` rejects an
`Age` without a rule of its own.

| Call on `Age`s                   | Result                                      |
| -------------------------------- | ------------------------------------------- |
| `max(a, b)`, `ages.sort()`       | `Age`; `Ord` only reads                     |
| `${age}`, a map key, `Serialize` | admitted; each trait only reads             |
| `T::default()` with `T = Age`    | error; `Default` produces a `Self`          |
| `twice(age)`                     | error; `Add`'s `Output` is `i32`, not `Age` |
| a sum of `ages`                  | written over the base: `ages.map(...)`      |

### A value enters a refined type only by an explicit conversion

Three explicit forms produce a refined value from an unrefined one, and nothing
else does:

- `R::from(b)` returns the `R` under a contract that states the clauses. An
  unmarked call traps on a failed clause, reporting it and blaming the caller.
  `unchecked R::from(b)` vouches for them, and only a build that checks
  contracts checks it.
- `R::try_from(b)` returns a `Result`. Its error names the clause that failed,
  through `Refine::describe` or the condition's text.
- A type pattern `p: R` matches where every clause holds, and binds `p` as an
  `R`, as it narrows a resource to a child. `let n: UserName = s else { … }`
  is the refutable form.

A newtype over a refined type has the same three, from the refined type's base:
`UserName::try_from(s)` takes a `String`.

An inline refined type has no name to call `from` or `try_from` on. Code that
builds one from its base uses a type pattern, or names the type with a newtype.
No syntax names an inline type for a call, and no conversion infers its target
from the expected type, for the reason
[Conversion Traits](./wep-2026-03-16-conversion-traits.md) gives against
`.into()`: the target would not appear in the text.

Narrowing is never implicit. A `B` where an `R` is expected is a type error, in
an assignment, an argument, a field initializer, a `return` and a literal
coercion alike. An implicit narrowing could only trap on a failed clause, and a
program cannot handle a trap: it ends the instance. A form field that fails its
clause would turn an HTTP handler's validation into a crashed request. The
fallible path has to be the one a program writes, so every narrowing is written
where the reader sees it.

A narrowing the compiler can prove is no exception: `let a: Age = 5;` is a type
error, though `5` is plainly in `0..=150`. Admitting it would make whether a
program compiles depend on what the compiler can prove, which grows release by
release.

`R::from` with a constant argument that fails a clause, such as
`UserName::from("")`, is a warning where the compiler evaluates the clause on
it, and the call still traps at run time. It is not yet an error, because niri
does not evaluate every clause.

### Two refined types of one shape are one type

Two refined types are the same type when they have the same base and the same
clauses. `String contract Length { max: 200 }` written on two fields is one
type, so a value moves between the fields with no check. Between two refined
types that differ, a value widens to the base and enters the other type through
one of the three forms, as any base value does.

Two newtypes over one refined type are still two types, as two newtypes over
one base are.

### What can be refined

A base that is a resource, holds one at any depth, or is
[affine](./spec-types.md#affine-types) cannot be refined, and `B contract C`
over one is a compile error.

A resource's state lives in the host: `contract self.is_open()` can turn false
when the host closes the file, with nothing on the Wado side entering the type
again.

Nearly every affine type holds a resource, so the first rule refuses it
already. The rest are refused because `try_from` consumes its argument: a failed
conversion would drop the value it could not refine. No program needs one yet,
and admitting affine bases later breaks no program that compiles today.

### Serialization enforces the clauses

`Serialize` writes a refined value as its base. `Deserialize` reads the base and
enters the refined type through `try_from`. A failed clause is a
`DeserializeError` carrying the path to the field. A field written
`bio: String contract Length { max: 200 }` is therefore checked by the derived
`Deserialize` of the struct holding it, with no library involved.

### Reflection reads the clauses

A refined type is a kind of its own in [Static Reflection](./spec-reflection.md).
`ReflectRefined` carries `Base` and `contracts()`, a tuple with one handle per
clause, in the order written. A value clause's handle returns the value at its
own type, so a library dispatches on it with an ordinary trait bound. A
condition clause's handle carries its source text and nothing to evaluate, so
the library can report what it cannot describe. `TypeInfo`
([Total Reflection](./wep-2026-09-05-total-reflection.md)) gains a `Refined`
case.

This is what Jade reads. `Length { min: 1, max: 64 }` becomes
`minLength: 1, maxLength: 64` through Jade's own impl for `Length`, and a
clause type that Jade does not know fails the derivation at compile time,
rather than quietly leaving the keyword out.

### Syntax

`contract` in a type is the contextual keyword it is in a signature.

A refined type in a return position is written in parentheses,
`-> (String contract Length { max: 3 })`. Without them, the clause would read
as the function's own `contract` clause. Any other place where a clause's end
is ambiguous is resolved the same way.

A function's clause is parsed as an `if` condition is, with no struct literal,
because the body's `{` follows it. A type's clause is not followed by a body, so
a value clause such as `Length { max: 3 }` needs no parentheses.

## Known gaps

### A base that reaches shared state

A clause is checked when a value enters the type. The `&mut` rule keeps a
refined value from changing through itself, and value semantics keep a copy
from changing through anything else. What a reference reaches is shared,
though, and it can change through another reference to it:

```text
struct Window { rows: &List<i32> }
type NonEmpty = Window contract self.rows.len() > 0;

let mut xs: List<i32> = [1];
let w = NonEmpty::from(Window { rows: &xs });   // the clause holds
xs.clear();                                      // and now it does not
```

A function value may read state it captured, so a clause calling one has the
same problem. A resource does too, since its state lives in the host, and so a
resource is never refined (see [What can be refined](#what-can-be-refined)).

### A default for an inline refined field

A field default is an expression, and the three forms that enter a refined type
give no expression for an inline one: `from` and `try_from` need a name, and a
type pattern is not an expression. `bio: String contract Length { max: 200 } = ""`
is a narrowing the rules refuse, so such a field takes no default unless its
type is named by a newtype.

### What makes two clauses the same

A refined type's identity is its base and its clauses. Nothing states yet
whether two value clauses are the same when their values are equal, whether two
conditions are the same when they read alike, or whether the order of the
clauses matters.

### The common clause types

`Length`, the ranges and a pattern match are the clauses most programs write.
Which of them the standard library provides, and the module that holds them,
are open. A pattern clause needs a regular expression engine, which the standard
library does not have. `Length` must also say what it counts: `String::len()`
counts bytes, and JSON Schema's `minLength` counts code points.

## Related WEPs

- [Contract Checks](./wep-2026-10-06-contract-checks.md) — the `contract` and
  `unchecked` this extends from calls to values
- [Newtype Semantics](./wep-2026-01-29-newtype-semantics.md) — the construct a
  refined type is not
- [Resource Inheritance](./wep-2026-04-28-resource-inheritance.md) — the
  subtyping rules a refined type reuses
- [Declared Absence](./wep-2026-09-13-declared-absence.md) — the diagnostic for a
  mutating method a refined value cannot call
- [Total Reflection](./wep-2026-09-05-total-reflection.md) — the `TypeInfo` case
  a refined type adds
- [Jade](./wep-2026-06-13-jade.md) — the first reader of the clauses
