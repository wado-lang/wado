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
| any other world at `-O0`           | on      |
| any other world at `-O1` and above | off     |

The test world defaults on whatever the level, because finding bugs is what it
is for. It already pays for that with the `debug` allocator.

### `builtin::contract_checks()`

`builtin::contract_checks()` answers whether the build checks contracts. A
function checks its contract with it, and this is the only shape it takes: the
whole condition of an `if` statement with no `else`. A call anywhere else is a
compile error.

```text
if builtin::contract_checks() {
    assert 0 <= index < self.len(), "index out of bounds";
}
```

It is the mechanism, not a stopgap. A contract syntax, once one is designed,
lowers to it.

Before NIR, the compiler keeps such an `if` where checks are on and deletes it
outright where they are off, so a disabled check costs nothing at any
optimization level. Folding the condition to `false` was not enough: the
inliner sized the `if false` body before any pass pruned it, and
`Formatter::prepare_int_write` stopped being inlined at `-O2`. For the same
reason each site writes its check out rather than calling a shared helper: the
call to a helper a disabled check left empty would remain.

Coverage does not count a check as a branch, because its condition is a build
constant. Where the build keeps the check, its body is part of the enclosing
region. Where the build deletes it, under `-f no-contract-checks`, it holds no
line.

It is `pub`, since contracts are open to every program and this is what they
lower to. Written by hand, it is a removable `assert`. Every language that
offers one reports it used for checks that must hold in production, and
removed with them (`assert` under Python's `-O`, Rust's `debug_assert!` in
RUSTSEC-2025-0137). The `contract` syntax below is the form that keeps a check
unless the call is marked.

### A skipped check is not an assumption

A disabled check is not evaluated, and the compiler does not assume its
condition holds. Assuming it would turn a violation the specification calls
unspecified, such as `get_unchecked` out of range, into unconstrained behavior.
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

`Sequence::get_unchecked` on an `Array` checks nothing: the array access
already traps out of range.

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
defined, as a default argument's expression is. A predicate may therefore
name a private field, though the caller could not write it.

A marked call outside the contract, in a build that does not check, is never
undefined behavior. It has the class the function states. An unconstrained
violation is still a bug that must not reach production, but the four
guarantees of [Behavior Classes](./spec-overview.md#behavior-classes) still
hold.

We know of no language with this design. The nearest is Rust, whose caller
writes `unsafe { … }` and whose standard library checks `unsafe` preconditions
according to the caller's `debug_assertions`. Rust's precondition is prose,
though, and breaking it is undefined behavior.

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
precondition of `from_utf8_unchecked`. A broken type invariant is also what
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
    contract 0 <= start <= end <= self.len(), "byte range out of bounds"
    contract self.is_char_boundary(start) && self.is_char_boundary(end)
{
    …
}
```

A condition is parsed as an `if` condition is, with no struct literal, so the
body's `{` cannot be read as one. Contracts state inputs only, so a clause
names no result and no old value.

A condition performs no effect. It may call a function that has a contract of
its own; that call is an ordinary call, checked unless it is marked.

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
index read whose range check the caller vouches for. An index assignment takes
no marker. `unchecked xs[i] = v` would put the operator on an assignment's
target, outside what a unary operator is, and is a compile error.

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
Any other `_unchecked` name is removed.

`unchecked` means that the caller does not ask for the check in a shipping
build. A build under `-f contract-checks` still checks a marked call. The word
was kept despite that because it continues the `_unchecked` names it replaces.

## Roadmap

1. `-f contract-checks`, `builtin::contract_checks()`, and the checks in
   `core:prelude`'s `_unchecked` functions. Done. The first run of the test
   suites found one violation, in the standard library itself:
   `StrSlice::replacen` copied a non-matching byte at a time, so the tail view
   it appended could start inside a character.
2. `contract` clauses and the `unchecked` operator. Done. A function with a
   contract is reified twice: itself, which always checks, and a twin named
   with an `$unchecked` suffix that checks under `builtin::contract_checks()`.
   A marked call reaches the twin. The parser puts each clause at the head of
   the body as an `assert` marked as a contract, so every pass over a body
   sees it, and the formatter prints it back as a clause.
3. Migrate `core:*`'s `_unchecked` functions to contracts, keeping an
   `#[unavailable]` declaration where Rust has a method of the same name.
4. The generated WIT carries an `export fn`'s contract as a comment.

## Known gaps

- A failed check reports the clause's position inside the function, not the
  call that broke the contract.
- A function reached by both a marked and an unmarked call is emitted twice,
  once for each.
- A contract on a trait's method, or on a declaration with no body, is a
  compile error.
- A clause that costs more than the function it guards is not checked: that
  the bytes are UTF-8 in `push_bytes_unchecked` and `from_utf8_unchecked`, and
  that a write keeps them UTF-8 in `set_byte_unchecked`.
- A call whose callee is not known statically, through a trait bound or a
  function value, has no call site to place the check at. How a trait
  method's contract binds an impl is not settled either, and `unchecked xs[i]`
  reaches an indexing trait's method.
- An input contract cannot say that a method taking `&mut self` keeps its
  type's invariant. It can only require inputs that imply it.
