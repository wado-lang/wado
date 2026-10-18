# WEP: Contract Checks

## Context

`assert` is never removed. That is deliberate: an invariant an `assert` states
must hold in the build that ships, where the code runs for real. C++26's
contracts are criticized for the opposite choice, an "ignore" semantic that
removes checks in production
([Research: Assertions and Contracts](./research-assertions-and-contracts.md)).

The `_unchecked` functions are the other side of that choice. They exist for
speed, and tuning `benchmark/` showed they pay for themselves. Each states a
contract in its doc comment, and a call outside it is a contract violation
([Behavior Classes](./spec-overview.md#behavior-classes)). Nothing checks the
contract in any build, so such a call is easy to write and invisible in tests.

The specification already permits any build to detect a contract violation
and trap. No build did.

## Decision

A build may check the contracts of `_unchecked` functions. Shipping code keeps
the speed it has today, and a build meant to find bugs traps on a violation.
This only adds checks: no build loses one it had, so C++'s objection to the
ignore semantic does not apply.

### The flag

`-f contract-checks` turns the checks on, and `-f no-contract-checks` turns
them off. The default depends on the world and the optimization level:

| Build                              | Default |
| ---------------------------------- | ------- |
| the test world (`wado test`)       | on      |
| any other world at `-O0` or `-O1`  | on      |
| any other world at `-O2` and above | off     |

The test world defaults on whatever the level, because finding bugs is what it
is for. It already pays for that with the `debug` allocator.

`wado run` and `wado serve` default to `-O1`, so a program being developed is
checked without asking. They compile what they run at once, and the compile
counts against the run: `-O2` costs more there than it saves, as the corpus
checks that run Gale tools found before they moved to `-O1`. A build that ships
comes from `wado compile` or `wado build`, which default to `-O2`. The checks
first defaulted on at `-O0` alone, and `wado run` at `-O2` left them off in the
loop where a developer meets a violation first.

### No program writes a removable check

The flag reaches only the code the compiler writes for a marked call. No
program can ask whether the build checks contracts. A check a program could
make conditional on the flag would be a removable `assert`. Every language that
offers one reports it used for checks that must hold in production, and
removed with them (`assert` under Python's `-O`, Rust's `debug_assert!` in
RUSTSEC-2025-0137). The `contract` syntax below is the only way to make a check
removable, and only a marker at the call removes it.

The first implementation offered `builtin::contract_checks()` as that
question, and the standard library's `_unchecked` functions wrapped their
checks in it. It was removed once the `contract` syntax could state every one
of those checks.

Before NIR, the compiler keeps the checks of a marked call where checks are on
and deletes them outright where they are off, so a disabled check costs
nothing at any optimization level. Folding a condition to `false` was not
enough: the inliner sized the `if false` body before any pass pruned it, and
`Formatter::prepare_int_write` stopped being inlined at `-O2`.

### A skipped check is not an assumption

A disabled check is not evaluated, and the compiler does not assume its
condition holds. Assuming it would turn a violation the specification calls
unspecified, such as `unchecked xs.at(i)` out of range, into unconstrained
behavior.
Zig's `std.debug.assert` and Swift's `-Ounchecked` do that, and their users
report the damage as worse than removing the check.

### A failed check is a failed assertion

A check is an `assert`, so it fails as one does: it reports the condition and
its operands, then traps. `-f bare-asserts` drops the message as it does for
any assertion.

### What is checked

Each `_unchecked` function checks the clauses of its contract that cost
constant time: an index in range, an end on a character boundary, a value that
is a Unicode scalar, a byte below `0x80`. A function that forwards to another
`_unchecked` function relies on that one's check.

### The caller decides whether a check can be removed

A checked function and its `_unchecked` twin state the same contract. They
differ in who guarantees it. The checked function guarantees a trap on a call
outside the contract, and its caller may rely on that trap, so no build may
remove it. The `_unchecked` twin leaves the guarantee to its caller, so its
check only diagnoses the caller's bug, and a build may remove it.

Whether a check can be removed is therefore a property of the call, not of the
function. A contract is declared once, on the function. A call without a
marker always checks it. A call with a marker states that the caller
guarantees the contract, and that call is checked only where
`-f contract-checks` is on. An `_unchecked` twin that is its checked function
minus the check becomes a marked call to the checked function.

The author of a function never chooses whether its check can be removed, so
the misuse of a removable `assert` cannot arise: removing a check takes a
marker that is visible at the call. A call from another component carries no
marker, so a contract on an `export fn` is always checked at the boundary.

The check runs at the call. It reports the caller's position and blames the
caller. Its predicate is evaluated in the context where the function is
defined, as a default argument's expression is.

A marked call outside the contract, in a build that does not check, is never
undefined behavior. It has the class the function states. An unconstrained
violation is still a bug that must not reach production, but the four
guarantees of [Behavior Classes](./spec-overview.md#behavior-classes) still
hold.

We know of no language with this design. The nearest is Rust, whose caller
writes `unsafe { … }` and whose standard library checks `unsafe` preconditions
according to the caller's `debug_assertions`. Rust's precondition is prose,
though, and breaking it is undefined behavior.

### A caller vouches only for what it can observe

Everything a clause names is at least as visible as the function it guards.
A `pub fn` cannot state its contract with a private field or a private
helper, and a clause that does is a compile error. The caller owes the
contract, so the caller must be able to evaluate it: to test it before the
call, or to show why it holds before marking the call. A predicate the caller
cannot read leaves `unchecked` to a guess. Eiffel states the same rule for its
preconditions. For the same reason, `wado doc` and the editor's hover show a
function's clauses beside its signature.

The same reasoning rules out a contract on input the caller cannot observe
before the call. An iterator may perform I/O, so its items exist only once the
callee pulls them. A caller that marked such a call would vouch for data it
has not seen, often data from outside the program. A function taking an
iterator therefore has no contract on its items. It validates them as it reads
them and reports a failure as an error.

### A contract states inputs only

A contract states preconditions and nothing else. A postcondition is the
callee's own obligation, so a caller's marker cannot vouch for it, and the
mechanism above could never remove its check. An `assert` in the body already
states it, checked in every build.

The code shows little demand for more. In `core:*`, Gale and Loam, the
`assert`s placed just before a `return` are either preconditions on the
arguments or invariants of a value the body computed, such as the normalized
mantissa in `int128.wado` or the loop entry `plus_body_entry` finds in Gale.
Neither states what the result means to the caller.

What a function guarantees about its result is better stated as a type: a type
whose invariant holds. Constructing such a type has the invariant as its
precondition, so its contract is again an input contract, and the same
mechanism checks it. `String` is the example: its UTF-8 invariant is the
precondition of `String::from_utf8`. A broken type invariant is also what
makes a violation unconstrained, since other code relies on the invariant.

Today such a type is a `struct` with a private field and a checked
constructor. A newtype that carries an invariant would state the same at no
cost, but it redesigns newtypes: `as` would convert only toward the base type,
and a conversion toward the newtype would go through `TryFrom`. That belongs
in a proposal of its own.

### Syntax: `contract` and `unchecked`

A function declares its contract with `contract` clauses, placed after the
`with` clause and before the body, or before the `;` of a trait method without
one. Each clause is a condition and an optional message, as an `assert` is,
and a function may have any number of them:

```text
pub fn slice(&self, start: i32, end: i32) -> StrSlice
    contract 0 <= start <= end <= self.len()
    contract self.is_char_boundary(start) && self.is_char_boundary(end)
{
    …
}
```

A condition is parsed as an `if` condition is, with no struct literal, so the
body's `{` cannot be read as one.

Clauses are separated by nothing but the `contract` that opens each, as `->`
and `with` open theirs. A comma between them would also be the comma before a
message, and brackets around each would set the clause apart from the rest of
the signature. Run together on one line they read poorly, so the formatter
puts each clause on a line of its own. Contracts state inputs only, so a clause
names no result and no old value.

A condition performs no effect. It may call a function that has a contract of
its own; that call is an ordinary call, checked unless it is marked.

A condition also writes only the values it creates. A skipped clause must
leave the call computing what a checked one computes, and a write to a
parameter, a global, or anything a reference reaches shows in one build and not
the other. The first implementation admitted such a write: a clause doubling its
parameter made `doubled(5)` return 10 at `-O0` and `unchecked doubled(5)` return
5 at `-O2`.

Every program may declare a contract, not only the standard library.

A caller marks a call with `unchecked`, a prefix unary operator. Postfix
operators bind tighter than prefix ones, so `unchecked a.f().g()` marks `g`,
as `-a.f().g()` negates the whole chain. The operand must be a call to a
function with a contract; anything else is a compile error.

```text
let b = unchecked s.get_byte(i);
let v = (unchecked s.slice(a, b)).to_string();
let x = unchecked xs[i];    // an index read with no range check
```

`xs[i]` is a call to an indexing trait's method, so `unchecked xs[i]` is an
index read whose range check the caller vouches for. It is still the place
`xs[i]` names, so `&mut (unchecked xs[i])` references through `IndexRefMut` where
`xs` is mutable and is an error where it is not. An index assignment takes no
marker: `unchecked xs[i] = v` and `unchecked xs[i] += v` are compile errors,
whatever contract `IndexAssign` declares.

`contract` is a contextual keyword: no identifier can follow a signature, so
code that names something `contract` keeps working. `unchecked` is reserved.
As a contextual keyword it would be ambiguous: `unchecked(x)` and
`unchecked[i]` already mean a call and an index on something named
`unchecked`.

The two words are asymmetric by design. `contract` names what the function
declares, an obligation. `unchecked` names what the caller gives up, the
check, and the contract stays in force. `check` and `unchecked` would read as
a clause and its negation, as if the marker removed the contract. `check` is
also a verb, so it reads as a statement run at that point, like `assert`.

A contract clause is not a coverage target, whether the call checks it or not.

A contract on an `export fn` has no place in WIT. The generated WIT carries it
as a comment on the function.

An `_unchecked` function becomes an `unchecked` call to its checked twin. The
old name stays, as an `#[unavailable]` declaration pointing at the new call,
only where Rust has a method of that name, such as `get_unchecked` or
`from_utf8_unchecked`, since that is the name a reader from Rust reaches for.
Any other public `_unchecked` name is removed.

`Sequence::get_unchecked` has no checked twin to mark: a generic body over
`Sequence` has no index operator. It becomes `Sequence::at`, which returns the
element under `contract 0 <= index < self.len()`, written on the trait. A
concrete `List`, `Slice` or `ByteList` reads with `unchecked xs[i]`, so
`ByteList::get_byte_unchecked`, a name Rust lacks, is removed. `at` is a
prelude name, so a program's own trait method `at` on one of these types makes
a call ambiguous until the call names the trait. That cost was accepted for the
short name.

No name carries `_unchecked`, so each operation has one name per shape:

| Shape                                | Name                 | Example                          |
| ------------------------------------ | -------------------- | -------------------------------- |
| Returns the value, under a contract  | the plain name       | `from_utf8`, `byte_at`, `xs[i]`  |
| Returns `Option` or `Result` instead | `try_`, or `get`     | `try_from_utf8`, `xs.get(i)`     |
| Skips the contract's check           | `unchecked` the call | `unchecked String::from_utf8(b)` |

Where the plain name returned an `Option`, the contract form takes it over:
`char::from_u32` returns a `char`, and the `Option` moves to
`char::try_from`. A reader from Rust who writes
`if let Some(c) = char::from_u32(v)` gets a type error rather than a different
behavior.
`String::byte_at` is a method of its own rather than `as_bytes()[i]`, which a
marked call would spell `unchecked s.as_bytes()[i]` at every byte scan.

`unchecked` means that the caller does not ask for the check in a shipping
build. A build under `-f contract-checks` still checks a marked call. The word
was kept despite that because it continues the `_unchecked` names it replaces.

### Who states a trait method's contract

A trait method's clauses are written on the trait's declaration, a signature
without a body included, and bind every impl, as its default arguments do.
Where the trait states clauses, an impl of the method writes none: it neither
restates them nor adds one. Where it states none, an impl may state its own, as
the last paragraphs of this section say. Trait dispatch is static, so a call
through a bound reaches a known impl, and each impl checks the trait's clauses
at its head.

An impl that added a clause to the trait's would make `unchecked` remove a
check its caller never saw: generic code marks a call vouching for the trait's
clauses, not for those of whichever impl it reaches. Removing a check takes a
marker at the call, and that marker means only what the call site can read.

A trait method that declares no clause leaves its impls free to state their
own. An unmarked call checks them however it reaches the impl, since static
dispatch reaches the impl's body after monomorphization. `unchecked` vouches
for them only where the call names the impl itself. Through a bound the
caller reads the trait's declaration, which states nothing, so `unchecked`
there is a compile error, for the reason above.

The indexing traits are the case. What an index must satisfy differs by
container: a list takes an index in range, a range index takes any ends since
it clamps them, and a map takes a key it holds. A trait clause would have to
call a predicate each impl defines, such as `contains_index`, and that
predicate is exactly the clause a caller cannot read: it says only "whatever
this impl accepts". A map would also look the key up twice on a checked read,
once for the predicate and once to read. So the indexing traits declare no
clause. `List` and `Slice` state `contract 0 <= index < self.len()` on their
index methods. `List` first stated only `index < self.len()`, leaving a
negative index to the array access's own trap, so that a guard proving only the
upper end removed the whole check. That made its hover and `wado doc` read as
if a negative index were admitted, while the same operation on a `Slice` read
otherwise. The clause now states both ends. The optimizer removes each half a
guard proves, so a guard proving only the upper end leaves one compare.
`Array` states none, since the Wasm array access traps on its own, and a map
states none, since a missing key panics as it always did.

An impl whose type cannot take every input its trait admits, where the trait
states a clause, is a design error in that impl, not a contract to add.

### A clause may cost as much as the function

The first checks covered only the clauses that cost constant time, because a
check written with `builtin::contract_checks()` ran in every checking build at
every call, marked or not. A clause is checked on two paths: an unmarked call, whose caller
asked for the check, and a marked call in a build that checks contracts. A
clause that costs the same order as the function's own work, such as
validating the bytes a function copies, is therefore written and checked. It
slows a checking build by a constant factor, and a marked call in a shipping
build does not pay it.

A clause that would raise the function's complexity is a type to design, not a
contract to write. A binary search that requires its input sorted takes a
sorted list type, which checks the order once when it is built.

### UTF-8 in the standard library

The `String` functions that take bytes follow from the rules above:

- `String::try_from_utf8` takes an `AsByteSlice` and returns a `Result`. It
  takes over from the `Result`-returning `String::from_utf8` and from
  `from_utf8_slice`. It validates the caller's bytes where they lie and copies
  them once. It reports invalid bytes rather than requiring valid ones, so it
  has no contract and no marked form. An iterator of bytes is collected first;
  a form taking one waits until a caller needs it.
- `String::from_utf8` takes an `AsByteSlice` and returns a `String`. It
  declares `contract String::is_valid_utf8(…)`, so an unmarked call traps on
  invalid bytes, and `unchecked String::from_utf8(bytes)` replaces
  `from_utf8_unchecked`.
- `String::push_bytes` declares that the appended bytes are valid UTF-8 on
  their own, and `unchecked` replaces `push_bytes_unchecked`. The old contract
  asked only that the result stay valid, which admits half a character now and
  the rest later. No caller relies on that.
- `set_byte_unchecked` becomes `internal_set_byte_unchecked`, an `internal`
  function with no contract. A character written a byte at a time is invalid
  UTF-8 between the writes, so no contract on one write can state what the
  sequence keeps. The name says what a reader needs: a standard library
  primitive that skips checks for speed, like `internal_reserve_uninit`. The
  standard library's tests hold the UTF-8 invariant, and the optimizer's append
  fusion still emits it.

No public function rewrites the bytes of an existing `String` in place. A
program that builds text a byte at a time writes a `ByteList`, which has no
invariant to break, and converts it once with `String::from_utf8`. An in-place
API waits for a program outside the standard library that needs one.

## Roadmap

1. `-f contract-checks`, `builtin::contract_checks()`, and the checks in
   `core:prelude`'s `_unchecked` functions. Done. The first run of the test
   suites found one violation, in the standard library itself:
   `StrSlice::replacen` copied a non-matching byte at a time, so the tail view
   it appended could start inside a character.
2. `contract` clauses and the `unchecked` operator. Done. A function with a
   contract is reified twice: itself, which always checks, and a twin named
   with an `$unchecked` suffix whose checks the flag keeps or deletes before
   NIR. A marked call reaches the twin. The parser puts each clause at the head of
   the body as an `assert` marked as a contract, so every pass over a body
   sees it, and the formatter prints it back as a clause.
3. A trait method's contract: written on the trait, checked in every impl and
   default body. Done. The parser keeps the clauses in a hidden default method
   of the trait, `$contract$<method>`, which takes the method's parameters.
   Reify opens the method and each impl's method with a call to it, and their
   `$unchecked` twins with a call to its twin, so a marked call through a bound
   reaches the twin of whichever impl the bound resolves to. The elaborator
   walks the holder, as it walks every default body, once in the trait's own
   frame with `Self` abstract, so the effect and purity checks and the language
   service answer for a trait that no impl takes.
4. A clause naming anything less visible than its function is a compile
   error, so a caller can always evaluate the contract it owes.
5. An `interface` operation's contract: written on the operation, checked in
   its default body and in every handler of it, and held to the purity check.
   Until then the parser rejects one.
6. An impl states its own clauses on a trait method that declares none, and
   `List` and `Slice` state the range on their index methods, so
   `unchecked xs[i]` is an index read with no range check. Done. The
   operand's dispatch already names the impl method, so the marker reaches its
   twin as it does for a call. `unchecked` through a bound whose trait method
   declares no clause is a compile error.
7. Migrate `core:*`'s `_unchecked` functions to contracts, keeping an
   `#[unavailable]` declaration where Rust has a method of the same name.
   - [x] The UTF-8 functions, in the shapes
     [UTF-8 in the standard library](#utf-8-in-the-standard-library) gives
     them.
   - [x] The functions with a checked twin: `slice`, `substr_bytes`,
     `truncate`, `insert`, `insert_str` and `remove` declare the contract,
     and `push_str_range_unchecked` becomes `push_str` of a marked `slice`.
   - [x] `builtin::contract_checks()` is removed. Each `_unchecked` function
     that still has no checked twin forwards to a private function that
     declares its contract.
   - [x] The functions with no checked twin, or one returning an `Option`:
     `String::push_ascii`, `char_at_byte`, `char::from_u32` and
     `String::byte_at` take a contract, and the `_unchecked` names go.
   - [x] `Sequence::get_unchecked` becomes `unchecked xs.at(i)`, and
     `ByteList::get_byte_unchecked` becomes `unchecked xs[i]`.
8. The generated WIT carries an `export fn`'s contract as a comment.
9. A failed check reports the call's position. Done. Reify splits a function
   with a contract in two rather than copying its body: the `$unchecked` twin
   holds the body, and the function itself checks the clauses and forwards to
   the twin. A trait method's impl gets the holder call as its one clause, so
   the holder needs no twin. A function value and an export reach the
   function itself. After monomorphization, ahead of the plan,
   `lower/contract_checks.rs` expands each direct call into a call to the
   twin whose last argument evaluates the clauses once every argument is
   evaluated, so the call stays the place an index names. An argument that is a
   place stays in the call, its subscripts bound. Another is bound where it is
   evaluated: the last argument's by a `let` in its own block, which the clauses
   after it share, an earlier one's by assignment to a local of the function. A
   `let` there would scope the local out of the clauses' reach. Where the last
   parameter borrows, or no twin follows a trait's clauses, the arguments are
   bound ahead instead, and the clauses then the call form a block. A copy
   reports its call's position through the `$contract_site` intrinsic, which
   the function's own clauses fall back from to the clause's. A copy tests the
   clause's bare condition. What a failure reports moves into one function per
   clause, `F$contract_report{k}`, which evaluates the condition again with its
   power-assert captures and takes only the locals that reads, plus the site.
   Under `-f contract-checks` a marked call goes back to the function before
   monomorphization, and so is expanded as an unmarked one is.
10. A clause writing what it does not declare is a compile error. Done. The
    purity check reads the clause's writes from `write_sites.rs`, which the
    `self_comparison` lint shares, and keeps those landing in a value the clause
    creates that holds no `&mut`, resource, or function value. A function value
    is called only where the clause writes the closure, since one from outside
    may write through a `&mut` it captured, `fn` as well as `fn mut`. Lowering then copies an argument from the caller's place and
    evaluates a failed clause again for its report without a parameter having
    changed.

## Known gaps

- A call inside a closure, or in a global's initializer, is not expanded: it
  reaches the function itself, and a failure reports the clause's position. So
  is every call to a function whose clause holds a closure.
- A contract on a declaration with no body, outside a trait, is a compile
  error.
- A function a clause calls may write a mutable global: global access is not
  tracked yet, so nothing tells the clause's check that it does.
- A clause cannot call a method on a value holding a resource or a function
  value, even one that only reads a field. Such a method may perform a host
  call or call the function without declaring an effect (#2369), and the check
  reads no body.
- A clause that takes `&mut` of a value it declares and hands the reference on
  through a local, rather than as the argument `&mut x`, is rejected, though
  the write lands in the clause's own value.
- A call through a function value has no callee known statically, so it has no
  contract to check or to remove.
- `String::from_utf8` copies the bytes of a `ByteList` argument, though the
  argument is its own copy and its buffer could become the string's.
- An input contract states what one call requires. It cannot state an
  invariant that a sequence of calls breaks and restores, such as UTF-8 while
  a character is written a byte at a time. Nor can it say that a method taking
  `&mut self` keeps its type's invariant; it can only require inputs that
  imply it.
