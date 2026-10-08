# Research: Assertions and Contracts in Other Languages

Wado checks the contracts of its `_unchecked` functions at run time, in a build
meant to find bugs. Each contract is written twice: as prose in the function's
doc comment, and as a check in its body. The open question is whether a
contract gets syntax on the function's signature. There it would be an
executable comment: documentation that a reader, a tool and a checking build
all read from one place.

This survey records where Wado stood on 2026-10-08 (`037dba480`), how other
languages write contracts, and what their users and researchers say about the
choices. It feeds no WEP yet.

## Wado on 2026-10-08

### `assert`

[Assertions](./spec-assertions.md) holds the rule. No build removes an
assertion: at every optimization level the condition is evaluated, and a false
one traps. A program can therefore rely on `assert` to protect its state.

The only part a build may drop is the power-assert message.
`-f bare-asserts` (on at `-Os`) makes a failed assertion trap silently.

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
that operation states. It is always a bug, and each operation says which class
its violation has. Unconstrained behavior arises from a contract violation and
from nothing else.

### Contract checks

[Contract Checks](./spec-assertions.md#contract-checks) holds the rule, and
[WEP: Contract Checks](./wep-2026-10-06-contract-checks.md) says why.

- `-f contract-checks` and `-f no-contract-checks` choose whether a build
  checks. The test world checks by default at every optimization level, and any
  other world checks by default at `-O0` only.
- A checking build traps on a call outside the contract, as a failed assertion
  does, message included.
- A build that does not check evaluates nothing, and the compiler does not
  assume the contract holds. The violation keeps the class the function's
  documentation gives it.
- `builtin::contract_checks()` is the mechanism. It is `internal` to
  `core:builtin`, so only the standard library writes a check. Open to every
  program, it would be a removable `assert`.

A checked function looks like this today. The contract appears once as prose
and once as code:

```text
/// # Contract
/// - `0 <= index < self.len()`.
///
/// Any other `index` is a contract violation, and the result is
/// unspecified: a byte of the backing array past `len()`, or a trap.
pub fn get_byte_unchecked(&self, index: i32) -> u8 {
    if builtin::contract_checks() {
        assert 0 <= index < self.used, "index out of bounds";
    }
    return self.repr[index];
}
```

The WEP lists three known gaps that bear on syntax:

- A failed check reports the position inside the `_unchecked` function, not
  the call that broke the contract.
- A clause that costs more than the function it guards is not checked, such as
  "the bytes are UTF-8" in `from_utf8_unchecked`.
- Contracts have no syntax.

### `_unchecked` functions

[WEP: String API — checked / unchecked / internal Discipline](./wep-2026-05-16-string-checked-unchecked-discipline.md)
sets the pattern. A checked method is `assert` on its inputs followed by a call
to its `_unchecked` twin.

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
width, so overflow is not a contract violation. Division by zero and a signed
`MIN / -1` trap, as the Wasm instructions do.

### Effects

[WEP: A Function Without `with` Performs No Effects](./wep-2026-09-30-effect-free-functions.md)
makes a function without a `with` clause effect-free. The exception is an
`#[ambient]` function such as `log_stderr`. A predicate written in Wado can
therefore be checked for effects, though it can still trap: an index out of
range or a failed `assert` inside a called function does.

`with` already follows a function's return type, so any clause added to the
signature has to sit beside it.

## Assertions elsewhere

An assertion is a statement in the body. This section covers the statement
alone; contracts on the signature follow in the next one.

| Language | Check that always runs           | Check a build removes                         | What a removed check means                          |
| -------- | -------------------------------- | --------------------------------------------- | --------------------------------------------------- |
| Rust     | `assert!`                        | `debug_assert!`, by `debug_assertions`        | nothing                                             |
| Swift    | `precondition`, kept at `-O`     | `assert` at `-O`; both at `-Ounchecked`       | nothing at `-O`; assumed true at `-Ounchecked`      |
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
removing the check would.

Swift does the same at `-Ounchecked`. On swift-evolution, Joseph Lord and Dave
Abrahams argued that an assertion should never create undefined behavior.
Chris Lattner answered that `-Ounchecked` deliberately removes the guard
rails, and that nobody has to use it.

Rust keeps the assumption separate and explicit. `core::hint::assert_unchecked`
is an `unsafe fn`, and calling it with a false condition is immediate undefined
behavior.

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

A contract can live in one of four places: on the signature, in the body as a
call, in a comment or a separate block, or at a module boundary. Each has a
language that chose it on purpose.

| Language            | Where           | Precondition                          | Postcondition                  | Result and old value    | Checking                                     |
| ------------------- | --------------- | ------------------------------------- | ------------------------------ | ----------------------- | -------------------------------------------- |
| Eiffel              | signature       | `require x > 0`                       | `ensure Result > x`            | `Result`, `old`         | a monitoring level per class; off in release |
| Ada 2012            | signature       | `with Pre => X > 0`                   | `Post => F'Result > X'Old`     | `'Result`, `'Old`       | `Assertion_Policy` per kind; SPARK proves    |
| D                   | signature       | `in (x > 0)`                          | `out (r; r > 0)`               | named in `out`          | `-release` removes; checked in the callee    |
| C++26               | signature       | `pre (x > 0)`                         | `post (r: r > 0)`              | named in `post`         | one of four semantics, per translation unit  |
| Vala                | signature       | `requires (x > 0)`                    | `ensures (result > 0)`         | `result`                | at run time                                  |
| Spec#, Midori       | signature       | `requires x > 0`                      | `ensures return > x`           | `return`, `old(..)`     | Midori: always                               |
| Dafny, Verus        | signature       | `requires x > 0`                      | `ensures r > x`                | the result is named     | proved statically                            |
| Rust nightly        | attribute       | `#[requires(x > 0)]`                  | `#[ensures(\|r\| *r > x)]`     | a closure parameter     | only under `-Zcontract-checks`               |
| .NET Code Contracts | body, as a call | `Contract.Requires(x > 0)`            | `Contract.Ensures(...)`        | `Contract.Result<T>()`  | a binary rewriter; discontinued              |
| Scala               | body, as a call | `require(x > 0)`                      | `ensuring(r => r > x)`         | a lambda parameter      | always; Stainless verifies                   |
| Clojure             | function header | `{:pre [(pos? x)]}`                   | `{:post [(pos? %)]}`           | `%`                     | the `*assert*` flag                          |
| JML                 | comment         | `//@ requires x > 0;`                 | `//@ ensures \result > x;`     | `\result`, `\old`       | a separate tool (OpenJML)                    |
| ACSL (Frama-C)      | comment         | `/*@ requires x > 0; */`              | `/*@ ensures \result > x; */`  | `\result`, `\old`       | a separate tool (E-ACSL checks, WP proves)   |
| Python PEP 316      | docstring       | `pre: x > 0`                          | `post: __return__ > x`         | `__return__`, `__old__` | deferred, never adopted                      |
| Move                | separate block  | `spec f { requires x > 0; }`          | `ensures result > x;`          | `result`, `old`         | the Move Prover only; never at run time      |
| Racket              | module boundary | `(contract-out [f (-> positive? …)])` | in the same `->` or `->i` form | named in `->i`          | always, with blame                           |

Swift, Go and Zig have no contract syntax. Kotlin's `contract { … }` shares the
name only: a function tells the compiler what its result implies, for smart
casts, and nothing is checked at run time.

### On the signature

Eiffel introduced Design by Contract. A failed precondition is the caller's
fault, and a failed postcondition is the function's own. The split is why a
precondition sits on the signature, where the caller reads it. Eiffel's
interface view of a class shows the signatures with their contracts and hides
the bodies, so the contract is the documentation.

Ada 2012 added the `Pre` and `Post` aspects. `pragma Assertion_Policy` decides
per kind of assertion whether a run checks it (`Check`) or skips it (`Ignore`).
GNATprove, SPARK's prover, uses every contract whatever the policy says, so one
contract serves both testing and proof. Tucker Taft told Rust's language team
that contracts were the one Ada 2012 feature that got people to move off older
versions.

D's `in`, `out` and `invariant` are removed by `-release`, as `assert` is.
DIP 1009 (2018) gave them the expression form in the table.

Vala places `requires` and `ensures` between the parameter list and the body,
after any `throws` clause. A method may repeat either clause.

Spec# put `requires` and `ensures` on the signature of C#. Midori, an operating
system written in a Spec#-derived C#, kept that placement after trying the
alternatives (see [A contract API in the body failed](#a-contract-api-in-the-body-failed)).

Dafny and Verus write the same clauses, and prove them instead of running them.
Verus names the result in the return type: `fn f(x: u32) -> (r: u32)`.

### As attributes

Rust's nightly `contracts` feature (rust-lang/rust#128044) writes
`#[core::contracts::requires(...)]` and `#[core::contracts::ensures(...)]`. A
`requires` may hold a sequence of statements ending in a `bool`
(rust-lang/rust#144444). A run checks contracts only under `-Zcontract-checks`,
which is off by default. The project goal behind it is to specify the safety
conditions of every `unsafe` function in the standard library and verify them
with Kani, without changing the library's behavior or speed for anyone who has
not opted in.

### In the body, as calls

.NET Code Contracts wrote contracts as calls to a library: `Contract.Requires`
and `Contract.Ensures` at the top of the body. A separate tool rewrote the
compiled assembly, moving each postcondition to every exit. Microsoft
discontinued it and archived the repository.

Scala's `require` is an ordinary function, and `ensuring` is a method on the
result expression. Stainless reads both as a specification to verify.

### In comments and separate blocks

JML and ACSL are contracts written in comments, so the host language's compiler
never reads them. A separate tool checks them at run time (OpenJML, E-ACSL) or
proves them (Frama-C's WP). They are literally executable comments, and run only
when that tool runs.

PEP 316 (2003) proposed `inv:`, `pre:` and `post:` lines in docstrings. Its
stated reasons were better documentation and easier testing. It was deferred
after python-dev objected that docstrings would grow even longer. Its style
lives on in CrossHair, which reads contracts from docstrings, and in decorator
libraries such as icontract.

Move puts a contract in a `spec` block beside the function. The Move Prover
reads it, and no build runs it.

### At module boundaries

Racket attaches a contract where a module exports a function, so the check runs
when a value crosses the boundary, not on every internal call. Every check
records which party broke it: the caller for an argument, the module for a
result. This is what "blame" means in the research below.

## What the record shows

### Most contracts are null and range checks

| Study                                  | Corpus                                                | Finding                                                                                                                                            |
| -------------------------------------- | ----------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------- |
| Chalin (2006)                          | 85 Eiffel projects                                    | the earlier study of contract use that Estler et al. extend                                                                                        |
| Estler, Furia, Nordio, Piccioni, Meyer | 21 projects in Eiffel, C# and Java; 260 million lines | over 33% of program elements carry contracts in most projects; contracts change less often than the code; inheritance changes little               |
| Schiller, Donohue, Coward, Ernst       | 90 C# projects using Code Contracts                   | 68% preconditions, 26% postconditions; 75% check that a value is present, mostly non-null; suggested contracts did not broaden what people wrote   |
| Dietrich, Pearce, Jezek, Brada         | the 200 most popular Maven Central projects           | many mechanisms mixed in one program; fewer contracts than expected; projects that adopt contracts add more; some contracts break substitutability |

Midori reached the same figure from the inside. Joe Duffy reports that about
90% of its contracts were nullability and numeric ranges, and that about 90%
of the argument-validating exceptions in typical .NET code became
preconditions. He sums it up as "contracts begin where the type system leaves
off". Midori explored non-null and range types to absorb those checks, and
Duffy calls the failure to deploy non-null types system-wide one of his biggest
regrets.

### A contract API in the body failed

Fähndrich, Barnett and Logozzo argued in "Embedded Contract Languages" (SAC

2010. for writing contracts as ordinary code rather than in a new syntax. They
      observed that the number of specification languages roughly equals the number
      of tools that consume them. Code Contracts was that design.

Two reasons are given for its discontinuation. Contracts took effect only after
the separate rewriter ran, and build pipelines that skipped it silently lost
them. And users found some constraints they wanted could not be written, while
others they wrote turned out too strict and had to be removed.

Duffy's account of Midori, written by a team that had lived through Code
Contracts, gives the design reasons:

- An API put the contracts in the implementation, not in the signature. On the
  signature they appear in documentation and IDE tooltips, and tools can reason
  about them.
- A postcondition must hold on every exit path, which an API call at the top of
  the body cannot express.
- The compiler could remove a check it proved true, and report an error for one
  it proved false.

### One kind of contract, always checked

Midori first offered weak and strong contracts, and contracts compiled only in
debug builds. Developers misused the variants and could not tell when a
contract would be checked. Midori dropped them all and kept a single kind that
is always checked. A violation ends the process ("abandonment"). Duffy reports
that forcing the one choice produced healthier code.

Midori's assertions went the other way. They stayed library calls
(`Debug.Assert`, `Release.Assert`), not part of any signature, and the team
wrote more release assertions than debug ones.

### Removing checks in production is the central objection

The C++26 debate is about removal, not placement. P2900 lets a build choose an
ignore semantic, under which a contract is not checked at all. The objections
come from compiler, library and language designers:

- P3173 and P3506 (Gabriel Dos Reis, Microsoft) called the design not viable,
  because evaluating a contract predicate may itself have undefined behavior,
  and recommended against including it in C++26.
- P3573, "Contract concerns", has nine authors and collects the objections.
- P4334 (Stroustrup, Garcia, Falco, Spicer, Voutilainen; 2026-08-09) calls
  enabling contracts for testing and removing them in production "a 1980s
  approach". It quotes Eiffel's own documentation, which says that "when
  releasing the final version of a system, it is usually appropriate to turn
  off assertion monitoring", and compares the ignore semantic to wearing life
  jackets only near the coast.

The alternative those papers name is the hardened standard library: checks that
stay on in production, measured at about 0.3% overhead across Google's servers.

### A disabled contract drifts from the code

A contract that no build reads stops being checked against the code it
describes.

- Rust's nightly contracts are not parsed or type-checked when contract checks
  are off (rust-lang/rust#145229). A contract can name a parameter that no
  longer exists, and nothing reports it.
- Code Contracts lost every contract in a build that skipped the rewriter.
- JML and ACSL are checked only when their separate tool runs.

### Checking in the callee loses the caller's build

D runs an `in` contract inside the called function. A library vendor who ships
a `-release` build therefore ships it with every precondition removed, and the
library's users cannot turn them back on. The DIP 1009 discussion asked for the
check to move to the call site, which would make the contract part of the
signature. No DIP followed. P3267, on C++ implementation strategies, weighs the
same choice between checking at the call and checking in the callee.

Rust's standard library takes the caller's side for its `unsafe` preconditions.
`assert_unsafe_precondition!` is decided by the caller's `debug_assertions`
when the caller is monomorphized, so a release-built standard library still
checks when it is called from a debug build. Each check says what it guards:
`check_language_ub` for undefined behavior of the language itself, and
`check_library_ub` for a documented library precondition whose violation is not
immediately undefined behavior. The two correspond roughly to Wado's
unconstrained and unspecified violations.

### The predicate has its own failures

A contract predicate is code, and every language has to say what happens when
that code misbehaves.

- Side effects: C++ lets a predicate be evaluated more than once or not at all,
  so its effects are unreliable. Midori proved every contract free of side
  effects.
- Undefined behavior inside the predicate is the reason P3173 rejects C++'s
  design.
- An exception escaping the predicate becomes a contract violation in C++, so
  it can no longer be caught as one from the body could (P4308).
- C++ treats the variables a predicate names as `const`, so a predicate calling
  a non-`const` function fails to compile (P3261).
- Rust's tracking issue lists the same open questions: whether a contract must
  be pure, and what happens when it panics.

### Inheritance needs a rule

A method that overrides another may only weaken its precondition and only
strengthen its postcondition, or callers written against the parent break.
Eiffel writes this as `require else` and `ensure then`, and Ada as `Pre'Class`
and `Post'Class`. C++26 adopted an inheritance model into P2900R13 and removed
it before R14 for lack of consensus, so virtual functions cannot carry
contracts yet. Dietrich et al. found Java contracts that break the rule in
practice.

### The cost of a check can exceed the function

Some preconditions cost far more than the function they guard. In a July 2025
thread on Rust's internals forum, checking that bytes are UTF-8, an O(n) scan,
was called too slow even for a debug build, since `from_utf8_unchecked` is
O(1). Others answered that a checked twin, `String::from_utf8`, already exists
for whoever wants the check.

D's forum returns to the same dilemma from the other side: keep expensive
contracts and miss the performance requirement, or remove them and miss the
safety requirement.

Higher-order contracts cost more again. A contract on a function argument has
to wrap the function and check each later call. Takikawa et al. ("Is Sound
Gradual Typing Dead?", POPL 2016) measured slowdowns above 100x in Typed
Racket, where such contracts guard every boundary between typed and untyped
code.

### One contract for checking and proof

Kani, Prusti and Creusot each define their own contract language for Rust. A
2022 design meeting of Rust's language team discussed a contracts RFC draft
whose motivation was a shared one, and agreed to start with contracts checked
at run time before any static verification. The standard library verification
effort now reports 989 contract-verified proofs.

Ada shows the payoff: GNATprove reads the same `Pre` and `Post` a test run
checks.

### The theory: blame, and contracts as types

Findler and Felleisen ("Contracts for Higher-Order Functions", ICFP 2002)
extended contracts to functions passed as values, and introduced blame: each
failed check names the party that broke the agreement. Dimoulas, Findler,
Flanagan and Felleisen ("Correct Blame for Contracts", POPL 2011) showed that
the obvious semantics sometimes blame the wrong module, and gave one that does
not.

Greenberg, Pierce and Weirich ("Contracts Made Manifest", POPL 2010) compared
latent contracts, which are checks the type system does not see, with manifest
contracts, which are refinement types recording the checks a value has passed.
The two had been assumed interchangeable. They are not quite: translating one
into the other can make a program blame more.

### Contracts as documentation and as test oracles

Sean Parent and Dave Abrahams ("Better Code: Contracts", CppCon 2023) teach
contracts as a design method, and argue between them whether the code or the
documentation matters more.

Khlebnikov and Lakos (P1743, "Contracts, Undefined Behavior, and Defensive
Programming") distinguish a wide contract, which has no precondition, from a
narrow one. They argue that giving every input a defined behavior, as a null
check in `strlen` would, hurts performance and correctness alike.

John Regehr ("Use of Assertions") counts executable documentation of pre- and
postconditions among the benefits of assertions, and calls them a gateway to
formal methods.

Hillel Wayne ("Property Tests + Contracts = Integration Tests") uses contracts
as the oracle of a property test. The test asserts nothing itself, and fails
when any contract does.

## Questions this leaves for Wado

The survey bears on these questions. It answers none of them.

- What a contract on the signature means. Midori kept one kind, always checked.
  Wado's contract checks are the removable kind, and `assert` the kept kind.
  Every language whose removable check shares a spelling with a kept one
  reports it misused, and the reason `builtin::contract_checks()` is
  `internal` applies to any syntax that offers it to every program.
- Whether a disabled contract is still compiled. A contract that is type- and
  effect-checked in every build cannot drift; one that is skipped can.
- Where the check runs. A contract on the signature lets the compiler place the
  check at the call, which would report the caller's position and blame the
  caller. Wado compiles a whole program from source, so D's separate
  compilation problem does not arise.
- What the predicate may do. Wado's effect rule answers purity, except for
  `#[ambient]` calls and for a predicate that traps.
- How a trait method's contract binds an impl. A trait method's `with` clause
  already bounds every impl of it.
- How a postcondition names the result and an old value. Wado has no
  `result` keyword, and an old value is a copy under value semantics.
- What to do with a clause too costly to check. It could be written and never
  checked, or not written at all.
- Syntax. Ada's `with Pre => …` collides with `with` for effects. Vala, Dafny
  and Verus place `requires` and `ensures` between the signature and the body.
- How much the type system takes first. The studies find most contracts are
  null and range checks, and `Option` already absorbs the null ones.

## Sources

### Wado

- [Assertions](./spec-assertions.md)
- [WEP: Contract Checks](./wep-2026-10-06-contract-checks.md)
- [WEP: Behavior Classes](./wep-2026-10-05-behavior-classes.md)

### Languages

- [AdaCore: Design by contracts](https://learn.adacore.com/courses/intro-to-ada/chapters/contracts.html)
- [Ada 2012 RM 11.4.2: pragmas `Assert` and `Assertion_Policy`](https://www.adaic.org/resources/add_content/standards/12rm/html/RM-11-4-2.html)
- [SPARK user's guide: assertion pragmas](https://docs.adacore.com/spark2014-docs/html/ug/en/source/assertion_pragmas.html)
- [D: DIP 1009 review thread](https://lists.puremagic.com/pipermail/digitalmars-d/2017-June/269108.html)
- [D: DIP 1006 review](https://forum.dlang.org/post/p7k63r$mt2$1@digitalmars.com)
- [D bug 3407: `-safe -release` must keep all bounds checks](https://issues.dlang.org/bugs/3407/)
- [Vala: assertions and contract programming](https://docs.vala.dev/tutorials/main/04-00-advanced-features/04-01-assertions-and-contract-programming)
- [PEP 316: Programming by Contract for Python](https://peps.python.org/pep-0316/)
- [The Move Prover: a guide](https://osec.io/blog/move-prover)
- [Zig-DbC](https://hn.svelte.dev/item/44876174)
- [Visual Studio Magazine: reconsider using Code Contracts](https://visualstudiomagazine.com/articles/2017/04/01/reconsider-using-contracts.aspx)
- [Bandit B101: `assert_used`](https://bandit.readthedocs.io/en/1.7.9/plugins/b101_assert_used.html)
- [SQLite: the use of `assert()`](https://www.sqlite.org/assert.html)
- [Go FAQ: why does Go not have assertions?](https://go.dev/doc/faq#assertions)
- [swift-evolution: asserts should not cause undefined behaviour](https://lists.swift.org/pipermail/swift-evolution/Week-of-Mon-20151228/004997.html)
- [Swift asserts: the missing manual](https://blog.krzyzanowskim.com/2015/03/09/swift-asserts-the-missing-manual/)
- [Ziggit: key semantics of `std.debug.assert`](https://ziggit.dev/t/key-semantics-of-std-debug-assert/11123)

### Rust

- [rust-lang/rust#128044: the `contracts` tracking issue](https://github.com/rust-lang/rust/issues/128044)
- [rust-lang/rust#144444: statements in `requires`](https://github.com/rust-lang/rust/pull/144444)
- [rust-lang/rust#145229: disabled contracts are not type-checked](https://github.com/rust-lang/rust/pull/145229)
- [Rust project goal: instrument the standard library with safety contracts](https://goals.rust-lang.org/2025h1/std-contracts.html)
- [Rust lang-team design meeting on contracts, 2022-11-25](https://github.com/rust-lang/lang-team/blob/master/design-meeting-minutes/2022-11-25-contracts.md)
- [Verifying the Rust Standard Library](https://arxiv.org/html/2606.17374v1)
- [`core::ub_checks` and `assert_unsafe_precondition!`](https://doc.rust-lang.org/src/core/ub_checks.rs.html)
- [`core::hint::assert_unchecked`](https://doc.rust-lang.org/std/hint/fn.assert_unchecked.html)
- [Rust internals: unsafe assertion invariants](https://internals.rust-lang.org/t/unsafe-assertion-invariants/23206)
- [RUSTSEC-2025-0137](https://rustsec.org/advisories/RUSTSEC-2025-0137)

### C++

- [P2900R14: Contracts for C++](https://isocpp.org/files/papers/P2900R14.pdf)
- [P3173R0: P2900R6 may be minimal, but it is not viable](https://isocpp.org/files/papers/P3173R0.pdf)
- [P3506R0: P2900 is still not ready for C++26](https://open-std.org/JTC1/SC22/WG21/docs/papers/2025/p3506r0.pdf)
- [P3267R1: C++ contracts implementation strategies](https://open-std.org/jtc1/sc22/wg21/docs/papers/2024/p3267r1.html)
- [P4334R0: P2900 contracts' fundamental flaws](https://www.open-std.org/jtc1/sc22/wg21/docs/papers/2026/p4334r0.pdf)
- [P1743R0: Contracts, undefined behavior, and defensive programming](https://www.open-std.org/JTC1/SC22/wg21/docs/papers/2019/p1743r0.pdf)
- [P3471: standard library hardening](https://wg21.link/p3471r2)
- [cppreference: contract assertions](https://en.cppreference.com/cpp/language/contracts)
- [DevClass: contracts are in C++26 despite disquiet over their value](https://www.devclass.com/development/2026/04/01/contracts-are-in-c26-despite-disquiet-over-their-value/5213555)
- [The Register: C++26 approved](https://www.theregister.com/2026/03/31/cplusplus26_approved)
- [Better Code: Contracts (slides)](https://sean-parent.stlab.cc/presentations/2023-10-06-better-code-contracts/2023-10-06-better-code-contracts.pdf)
- [Krzemieński: Preconditions, part 1](https://isocpp.org/blog/2013/01/preconditions-part-1)

### Papers and essays

- [Joe Duffy: The Error Model](https://joeduffyblog.com/2016/02/07/the-error-model/)
- [Estler et al.: Contracts in Practice](https://arxiv.org/pdf/1211.4775)
- [AdaCore: studies of contracts in practice](https://blog.adacore.com/studies-of-contracts-in-practice)
- [Schiller et al.: Case Studies and Tools for Contract Specifications](https://homes.cs.washington.edu/~mernst/pubs/contract-specifications-icse2014.pdf)
- [Dietrich et al.: Contracts in the Wild](https://drops.dagstuhl.de/storage/00lipics/lipics-vol074-ecoop2017/LIPIcs.ECOOP.2017.9/LIPIcs.ECOOP.2017.9.pdf)
- [Fähndrich, Barnett, Logozzo: Embedded Contract Languages](https://www.microsoft.com/en-us/research/publication/embedded-contract-languages/)
- [Findler, Felleisen: Contracts for Higher-Order Functions](https://www.cs.northwestern.edu/~robby/pubs/papers/ho-contracts-techreport.pdf)
- [Dimoulas et al.: Correct Blame for Contracts](https://www.cs.northwestern.edu/~robby/pubs/papers/popl2011-dfff.pdf)
- [Greenberg, Pierce, Weirich: Contracts Made Manifest](https://cs.pomona.edu/~michael/papers/popl2010_contracts.pdf)
- [Is Sound Gradual Typing Dead? (summary)](https://blog.acolyer.org/2016/02/05/is-sound-gradual-typing-dead/)
- [Hillel Wayne: contract examples](https://hillelwayne.com/post/contract-examples/)
- [Hillel Wayne: posts on contracts](https://www.hillelwayne.com/tags/contracts)
- [John Regehr on assertions, discussed on the D forum](https://digitalmars.com/d/archives/digitalmars/D/John_Regehr_on_Use_of_Assertions_318515.html)
