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

| Build                                | Default |
| ------------------------------------ | ------- |
| the test world (`wado test`)         | on      |
| any other world at `-O0`             | on      |
| any other world at `-O1` and above   | off     |

The test world defaults on whatever the level, because finding bugs is what it
is for. It already pays for that with the `debug` allocator.

### `builtin::contract_checks()`

`builtin::contract_checks()` answers whether the build checks contracts. The
compiler replaces each call with `true` or `false` before NIR. Where checks
are off, it deletes an `if builtin::contract_checks() { … }` statement
outright, so a disabled check costs nothing at any optimization level. Folding
the condition alone was not enough: the inliner sized the `if false` body
before any pass pruned it, and `Formatter::prepare_int_write` stopped being
inlined at `-O2`.

A function checks its contract with it:

```text
if builtin::contract_checks() {
    assert 0 <= index < self.len(), "index out of bounds";
}
```

It is the mechanism, not a stopgap. A contract syntax, once one is designed,
lowers to it.

It is `internal` to `core:builtin`, so only the standard library calls it.
Open to every program, it would be a removable `assert`. Every language that
offers one reports it used for checks that must hold in production, and
removed with them (`assert` under Python's `-O`, Rust's `debug_assert!` in
RUSTSEC-2025-0137).

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

## Roadmap

1. `-f contract-checks`, `builtin::contract_checks()`, and the checks in
   `core:prelude`'s `_unchecked` functions. Done. The first run of the test
   suites found one violation, in the standard library itself:
   `StrSlice::replacen` copied a non-matching byte at a time, so the tail view
   it appended could start inside a character.

## Known gaps

- A failed check reports the position inside the `_unchecked` function, not
  the call that broke the contract.
- A clause that costs more than the function it guards is not checked: that
  the bytes are UTF-8 in `push_bytes_unchecked` and `from_utf8_unchecked`, and
  that a write keeps them UTF-8 in `set_byte_unchecked`.
- Contracts have no syntax. The clauses live in each function's body, and its
  doc comment restates them.
