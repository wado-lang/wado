# Research: Assertions and Contracts in Other Languages

Wado's `assert` is never removed, in any build. The question that started this
survey is a different kind of check: one that states a function's contract,
runs in a build meant to find bugs, and is skipped in a build meant to ship.
The `_unchecked` functions are the first candidates. Such a check could be a
second assertion statement, or a contract written on the function's signature.

This survey records where Wado stood on 2026-10-06 (`a03e903f9`), what other
languages chose, and what their users say about it. It feeds no WEP yet.

## Wado on 2026-10-06

### `assert`

[Assertions](./spec-assertions.md) holds the rule. No build removes an
assertion: at every optimization level the condition is evaluated, and a false
one traps. A program can therefore rely on `assert` to protect its state.

The only part a build may drop is the power-assert message.
`-f bare-asserts` (on at `-Os`) makes a failed assertion trap silently, and
compiles in nothing that captures operands or builds the message.

`panic(message)` and `unreachable()` trap on purpose. Nothing recovers from a
trap.

### Behavior classes and contract violations

[Behavior Classes](./spec-overview.md#behavior-classes) holds the rule, and
[WEP: Behavior Classes](./wep-2026-10-05-behavior-classes.md) says why.
Where the specification leaves behavior open, it names one of three classes:

- Unspecified: the outcomes are listed, the compiler picks one, and a compiled
  program keeps that pick on every host.
- Host-defined: unspecified, but the host may pick, so two hosts may differ.
- Unconstrained: no outcomes are listed, and the compiler may assume the case
  never arises. Four guarantees remain: the instance stays memory-safe,
  components stay isolated, no capability beyond the world's imports is used,
  and GC references stay well-typed.

A contract violation is a program reaching an operation outside the contract
that operation states. It is a second axis, independent of the class. It is
always a bug, and each operation says which class its violation has.
Unconstrained behavior arises from a contract violation and from nothing else.

Any build may detect a contract violation and trap. No build may trap on
unspecified or host-defined behavior that is not a contract violation. The WEP
lists the gap this question is about: no build detects a contract violation
yet.

### `_unchecked` functions

[WEP: String API — checked / unchecked / internal Discipline](./wep-2026-05-16-string-checked-unchecked-discipline.md)
sets the pattern. A checked method is `assert` on its inputs followed by a call
to its `_unchecked` twin. `String::truncate` is one:

```text
assert byte_len >= 0, "negative length";
assert self.is_char_boundary(byte_len), "not on a UTF-8 character boundary";
self.truncate_unchecked(byte_len);
```

Each `_unchecked` function in `core:prelude` states its contract in its doc
comment, most under a `# Contract` heading, and names the class of a
violation. The contract is prose, and only a person reads it.

| Function                     | Contract                                                   | Class of a violation |
| ---------------------------- | ---------------------------------------------------------- | -------------------- |
| `String::get_byte_unchecked` | `0 <= index < len()`                                       | unspecified          |
| `Slice::get_unchecked`       | `0 <= index < len()`                                       | unspecified          |
| `String::set_byte_unchecked` | `0 <= index < len()`, and the bytes stay UTF-8             | unconstrained        |
| `char::from_u32_unchecked`   | the value is a Unicode scalar value                        | unconstrained        |
| `StrSlice::slice_unchecked`  | `0 <= start <= end <= len()`, both on character boundaries | unconstrained        |

Wado has no `unsafe` block, and the behavior-classes WEP plans none.

### Arithmetic

[Overflow and Division by Zero](./spec-expressions.md#overflow-and-division-by-zero)
holds the rule. Integer `+`, `-` and `*` wrap in two's complement at every
width, and their overflow never traps. Overflow is therefore not a contract
violation in Wado. Division by zero and a signed `MIN / -1` trap, as the Wasm
instructions do.

### Effects

[WEP: A Function Without `with` Performs No Effects](./wep-2026-09-30-effect-free-functions.md)
makes a function without a `with` clause effect-free. The exception is an
`#[ambient]` function such as `log_stderr`, whose body is exempt from effect
checking. Whether an unused call to an ambient function runs is unspecified.

A predicate can still trap without performing an effect: an index out of range
or a failed `assert` inside a called function does.

### The test world

`wado test` compiles to the test world. It already differs from a shipping
build in one way: it uses the `debug` allocator, which never reuses freed
memory and poisons it with `0xFF`. The behavior-classes WEP names the test
world as a build that could keep the checks an `_unchecked` function elides.

## Assertions elsewhere

| Language | Check that always runs           | Check a build removes                         | What a removed check means                          |
| -------- | -------------------------------- | --------------------------------------------- | --------------------------------------------------- |
| Rust     | `assert!`                        | `debug_assert!`, by `debug_assertions`        | nothing                                             |
| Swift    | `precondition`, kept at `-O`     | `assert`, removed at `-O`                     | assumed true at `-Ounchecked`                       |
| Kotlin   | `require`, `check`               | `assert`, by the JVM's `-ea`                  | nothing                                             |
| Java     | none                             | `assert`, off unless `-ea`                    | nothing                                             |
| Python   | none                             | `assert`, removed by `-O`                     | nothing                                             |
| C++26    | hardened standard library        | `pre`, `post`, `contract_assert` under ignore | nothing                                             |
| D        | none                             | `assert`, `in`, `out`, removed by `-release`  | a violation is undefined behavior                   |
| Zig      | none                             | `std.debug.assert`                            | `unreachable`, so undefined behavior in ReleaseFast |
| Go       | none: the language has no assert | none                                          | none                                                |

### One spelling with two meanings gets misused

Where `assert` disappears in a release build, people still write it for checks
that must hold.

- Bandit's B101 warns on every Python `assert`. Projects had used `assert` to
  enforce interface constraints, and `-O` removed those protections.
- RUSTSEC-2025-0137 is a safe Rust function that relied on `debug_assert!` to
  reject bad input. A release build removed the check and left undefined
  behavior.
- Java's assertions are off unless `-ea` is passed, so in production they
  rarely run at all.

### A removed check that becomes an assumption does more damage

Zig implements `std.debug.assert` as `if (!ok) unreachable`. In ReleaseFast,
reaching `unreachable` is undefined behavior, and the optimizer uses it. Zig's
users point out that this does more harm, and is harder to debug, than
removing the check would. They also say the documentation's "optimized away"
reads as if the statement vanished.

Swift does the same at `-Ounchecked`: `assert` and `precondition` are not
evaluated, but the optimizer may assume they hold. On swift-evolution, Joseph
Lord and Dave Abrahams argued that an assertion should never create undefined
behavior. Chris Lattner answered that `-Ounchecked` deliberately removes the
guard rails, and that nobody has to use it.

Rust keeps the assumption separate and explicit. `core::hint::assert_unchecked`
(stable since 1.81) is an `unsafe fn`, and calling it with a false condition is
immediate undefined behavior.

### Keeping checks on costs little where it was measured

Google applied libc++'s hardening mode to hundreds of millions of lines of C++.
It found over 1,000 bugs, and the average slowdown was 0.30%. That experience
is the case for C++26's standard library hardening (P3471), whose checks
terminate the program in a hardened implementation.

### Removal done well gives the two kinds different names

SQLite's `assert(X)` states that the developers have a proof that X holds, and
it is a no-op in a release build: enabled, the asserts make SQLite about three
times slower. A check the developers believe but have not proven is written
`ALWAYS(X)` or `NEVER(X)` instead. Those stay in the release build, and code
after them handles the case where the belief was wrong.

### Some refuse assertions altogether

Go has no assertions. Its FAQ says programmers "use them as a crutch to avoid
thinking about proper error handling and reporting". SQLite's assert page
argues against that view.

## Contract syntax elsewhere

| Language     | Precondition         | Postcondition              | Status                                   |
| ------------ | -------------------- | -------------------------- | ---------------------------------------- |
| Eiffel       | `require`            | `ensure`, with `old`       | the origin of Design by Contract         |
| Ada 2012     | `with Pre => …`      | `Post => …`, with `'Old`   | in the language; SPARK proves them       |
| D            | `in (x > 0)`         | `out (r; r > 0)`           | in the language; `-release` removes them |
| C++26        | `pre (x > 0)`        | `post (r: r > 0)`          | adopted over sustained objection         |
| Rust nightly | `#[requires(x > 0)]` | `#[ensures(\|r\| *r > 0)]` | experimental (`#![feature(contracts)]`)  |
| .NET         | `Contract.Requires`  | `Contract.Ensures`         | discontinued                             |

Kotlin also has a feature named "contracts", but it is a different thing: a
function tells the compiler what its result implies, for smart casts. It
checks nothing at run time.

### Eiffel

Eiffel introduced Design by Contract. A failed precondition is the caller's
fault, and a failed postcondition is the function's own. That split is what
makes a precondition part of the signature rather than of the body.

### Ada and SPARK

Ada 2012 added `Pre` and `Post` aspects. `pragma Assertion_Policy` decides per
kind of assertion whether a run checks it (`Check`) or skips it (`Ignore`).
GNATprove, SPARK's prover, uses every contract whatever the policy says, so one
contract serves both testing and proof. Tucker Taft told Rust's language team
that contracts were the one Ada 2012 feature that got people to move off older
versions.

### D

D's `in`, `out` and `invariant` are removed by `-release`, as `assert` is. Its
forum returns to one dilemma: keep expensive contracts and miss the
performance requirement, or remove them and miss the safety requirement.

### C++26

P2900 adds `pre`, `post` and `contract_assert`, each evaluated under one of four
semantics chosen by the build: ignore, observe, enforce and quick-enforce. The
committee added it to the working draft in Hagenberg in February 2025 (100 for,
14 against, 12 abstaining). C++26 as a whole was approved in March 2026 (114
for, 12 against, 3 abstaining).

The objections did not go away. Bjarne Stroustrup co-wrote P3573 "Contract
concerns", said of the feature that "it's not minimal, it's not viable", and
said he will recommend not using it. The recurring complaints:

- The ignore semantic removes the checks in production, where the code runs
  for real.
- Different translation units can be built with different semantics, so one
  inline function can mean different things. Stroustrup objects to "changing
  the meaning of code depending on where it is".
- A predicate may have side effects, and the specification lets it be
  evaluated more than once or not at all, so those effects are unreliable.
- The standard library does not use it. libc++, libstdc++ and the Microsoft STL
  each harden with their own macros.
- Virtual functions cannot carry contracts yet.

### .NET Code Contracts

Microsoft discontinued Code Contracts and archived its repository. Two reasons
are cited. Contracts took effect only after a separate tool rewrote the
compiled assembly, and build pipelines that skipped that tool silently lost
them. And users found some constraints they wanted could not be written, while
others they wrote turned out too strict and had to be removed.

## What Rust's users asked for

### A shared contract language for verification tools

A 2022 design meeting of the Rust language team discussed a contracts RFC
draft. The motivation was that Kani, Prusti and Creusot each define their own
contract language. The draft proposed `contract` and `debug_contract`, mirroring
`assert!` and `debug_assert!`, so that expensive checks could be turned off.
The team agreed to start small, with contracts checked at run time, before any
static verification.

### The `contracts` feature

Contracts landed on nightly as an experiment (rust-lang/rust#128044, compiler
MCP 759) with `#[requires]` and `#[ensures]`. A run checks them only under
`-Zcontract-checks`, which is off by default. The project goal behind it is to
specify the safety conditions of every `unsafe` function in the standard
library and verify them with Kani. Its design rule is that the contracts change
nothing about the standard library's behavior or speed unless a user opts in to
checking them. The verification effort reports 989 contract-verified proofs.

The tracking issue's open questions overlap with Wado's:

- Whether a contract must be pure.
- What happens when a contract expression panics.
- Whether safety conditions and correctness conditions are written apart.
- What to do with a condition that only a run can check, or only a proof can.

### Checking `unsafe` preconditions in debug builds

The standard library already checks many `unsafe` preconditions with
`assert_unsafe_precondition!`. The check is decided by the caller's
`debug_assertions` when the caller is monomorphized, not by how the standard
library was built. A release-built standard library therefore still checks
when it is called from a debug build. Each check says what it guards:

- `check_language_ub` guards undefined behavior of the language itself.
- `check_library_ub` guards a documented library precondition whose violation
  is not immediately undefined behavior.

The two correspond roughly to Wado's unconstrained and unspecified violations.

### An O(1) function with an O(n) precondition

In July 2025 a thread on Rust's internals forum proposed writing each `unsafe`
precondition as a machine-readable `debug_assert`. The objection: some
preconditions are far costlier than the function they guard. Checking that the
bytes are UTF-8 is O(n), while `from_utf8_unchecked` is O(1), and such a check
was called too slow even for a debug build. Others answered that a checked
twin, here `String::from_utf8`, already exists for whoever wants the check.

### Integer overflow as a program error

Rust's RFC 560 makes integer overflow a program error that is not undefined
behavior. A build with `debug_assertions` must detect it and panic. A release
build may skip the check, and then the result is defined to wrap. In Wado's
terms, that is a contract violation whose class is unspecified, with exactly
one listed outcome.

A 2021 internals thread proposed checking overflow in every build. It stalled
on cost: one cited study measured slowdowns from 0.4% to 95%, with a mean of
30%. Wado took neither side: its arithmetic wraps by definition, so overflow is
not a violation at all.

## Questions this leaves for Wado

The survey bears on these questions. It answers none of them.

- Name. Every language whose `assert` disappears in a release build reports it
  being used for checks that must hold. A removable check needs a name that a
  reader cannot mistake for `assert`.
- What a skipped check means. Skipping can mean "not evaluated", or "assumed
  true". The second turns a violation whose class is unspecified into an
  unconstrained one, which is what Zig's and Swift's users complain about.
- Whose build decides. Rust decides an `unsafe` precondition check by the
  caller's build. C++ decides by translation unit and has to answer for mixed
  builds. Wado compiles a whole program from source, and has the test world.
- Purity of a predicate. Wado's effect rule answers what C++ and Rust leave
  open, except for `#[ambient]` calls and for a predicate that traps.
- Cost. Some preconditions cost more than the function they guard.
- Signature or body. A contract on the signature can be read by `wado doc`, the
  language service and a prover, and blames the caller by its position. A
  check in the body is a statement like `assert`, and needs no new grammar.
- Syntax. `with` already introduces a function's effects, so Ada's
  `with Pre => …` would collide with it.

## Sources

- [Rust project goal: instrument the standard library with safety contracts](https://goals.rust-lang.org/2025h1/std-contracts.html)
- [rust-lang/rust#128044: the `contracts` tracking issue](https://github.com/rust-lang/rust/issues/128044)
- [Rust lang-team design meeting on contracts, 2022-11-25](https://github.com/rust-lang/lang-team/blob/master/design-meeting-minutes/2022-11-25-contracts.md)
- [Verifying the Rust Standard Library](https://arxiv.org/html/2606.17374v1)
- [`core::ub_checks` and `assert_unsafe_precondition!`](https://doc.rust-lang.org/src/core/ub_checks.rs.html)
- [`core::hint::assert_unchecked`](https://doc.rust-lang.org/std/hint/fn.assert_unchecked.html)
- [Rust internals: unsafe assertion invariants](https://internals.rust-lang.org/t/unsafe-assertion-invariants/23206)
- [RUSTSEC-2025-0137](https://rustsec.org/advisories/RUSTSEC-2025-0137)
- [RFC 560: integer overflow](https://rust-lang.github.io/rfcs/0560-integer-overflow.html)
- [Rust internals: switch the default on overflow checking](https://internals.rust-lang.org/t/thought-switch-the-default-on-overflow-checking-and-provide-rfc-560s-scoped-attribute-for-checked-arithmetic/15118)
- [The `contracts` crate](https://docs.rs/contracts/latest/contracts/)
- [P2900R14: Contracts for C++](https://isocpp.org/files/papers/P2900R14.pdf)
- [cppreference: contract assertions](https://en.cppreference.com/cpp/language/contracts)
- [DevClass: contracts are in C++26 despite disquiet over their value](https://www.devclass.com/development/2026/04/01/contracts-are-in-c26-despite-disquiet-over-their-value/5213555)
- [The Register: C++26 approved](https://www.theregister.com/2026/03/31/cplusplus26_approved)
- [P3471: standard library hardening](https://wg21.link/p3471r2)
- [C++26: standard library hardening](https://www.sandordargo.com/blog/2026/05/13/cpp26-library-hardening)
- [swift-evolution: asserts should not cause undefined behaviour](https://lists.swift.org/pipermail/swift-evolution/Week-of-Mon-20151228/004997.html)
- [Swift asserts: the missing manual](https://blog.krzyzanowskim.com/2015/03/09/swift-asserts-the-missing-manual/)
- [Ziggit: key semantics of `std.debug.assert`](https://ziggit.dev/t/key-semantics-of-std-debug-assert/11123)
- [D: DIP 1006 review](https://forum.dlang.org/post/p7k63r$mt2$1@digitalmars.com)
- [D bug 3407: `-safe -release` must keep all bounds checks](https://issues.dlang.org/bugs/3407/)
- [Ada 2012 RM 11.4.2: pragmas `Assert` and `Assertion_Policy`](https://www.adaic.org/resources/add_content/standards/12rm/html/RM-11-4-2.html)
- [SPARK user's guide: assertion pragmas](https://docs.adacore.com/spark2014-docs/html/ug/en/source/assertion_pragmas.html)
- [Visual Studio Magazine: reconsider using Code Contracts](https://visualstudiomagazine.com/articles/2017/04/01/reconsider-using-contracts.aspx)
- [Bandit B101: `assert_used`](https://bandit.readthedocs.io/en/1.7.9/plugins/b101_assert_used.html)
- [SQLite: the use of `assert()`](https://www.sqlite.org/assert.html)
- [Go FAQ: why does Go not have assertions?](https://go.dev/doc/faq#assertions)
