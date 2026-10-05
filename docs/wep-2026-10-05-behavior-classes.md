# WEP: Behavior Classes

## Context

The specification leaves some behavior open on purpose, and names it
inconsistently.

- A call to an `#[ambient]` function whose result goes unused may be removed,
  so its output may never appear.
- Globals initialize in dependency order, but the order between two
  independent initializers is not stated.
- `ref_eq` on references to two distinct places may answer either way.
- A trap's message is called "implementation-defined".
- Relaxed SIMD edge cases are called "implementation-defined but consistent
  within a single runtime".
- `get_unchecked` called out of range "yields an unspecified value of the
  element type or traps".
- Other `_unchecked` functions say only that they leave a check to the caller.

There is no word for what these share, and no word for how they differ. C's
"undefined behavior" is too strong for any of them. Every Wado program runs in
a Wasm sandbox, so even a broken `_unchecked` contract cannot corrupt the
engine or escape the component. It is not safe either. Inside the instance it
can return wrong answers or leak data.

Wado has one implementation, so C's "implementation-defined" adds nothing. Its
point is that the implementation documents its choice.

## Decision

The specification uses three behavior classes and one term for a program
error. A class says what can happen. The term says whether reaching it is a
bug. The two are independent.

### Unspecified

The specification lists the permitted outcomes and does not say which one
occurs. The compiler chooses, so the choice may change with the optimization
level or the compiler version. Once a program is compiled the choice is fixed:
the same `.wasm` behaves the same on every host.

A correct program is correct under every listed outcome. After the point in
question, the program still follows the specification.

```wado
global A: i32 = trace("A", 1);   // trace logs through an ambient call
global B: i32 = trace("B", 2);
```

The log reads `A B` or `B A`. Either way `A == 1` and `B == 2` afterwards.

Unspecified behavior:

- whether an unused call to an `#[ambient]` function runs
- the order between initializers that do not depend on each other
- `ref_eq` on references to distinct places
- the text of a trap's message

### Host-defined

Unspecified, except that the host chooses rather than the compiler. The same
`.wasm` may behave differently on two hosts, so a golden test cannot pin it.
Each case states how consistent the choice is, since the cases differ.

Host-defined behavior:

- The bit pattern of a NaN a float operation produces. It may differ between
  two executions of the same operation. Only bit-level access such as
  `to_bits` observes it: the order on floats treats every NaN as one value.
- Relaxed SIMD results on edge-case inputs. The choice is fixed for a host.
- The value `InsecureSeed` returns. A host may return the same value every
  time.

### Unconstrained

The specification lists no outcomes. The optimizer may assume the broken
contract holds, and what follows from that assumption reaches beyond the
call: other functions, later in time.

Unconstrained behavior still keeps these guarantees:

1. The Wasm instance stays memory-safe. The engine's bounds checks are never
   elided.
2. Components stay isolated from each other.
3. No capability is used that the world's imports did not grant.
4. GC references stay well-typed.

Within those guarantees anything can happen: a trap, a wrong value, an
inconsistency found much later, a loop that never ends, or a leak of data
inside the instance.

```wado
let c = char::from_u32_unchecked(0xD800);   // a surrogate is not a char
let kind = match c {
    '\0'..='\u{D7FF}' => 0,
    '\u{E000}'..='\u{10FFFF}' => 1,          // exhaustive for every valid char
};
```

The `match` may take either arm or reach `unreachable`, depending on how the
optimization level lowers it. Pushing `c` into a `String` stores bytes that are
not UTF-8. From then on `chars()` disagrees with `len()`, a parser that trusts
UTF-8 may skip a closing quote, and a map lookup on the string may miss.

```wado
let buf = "user=alice;token=XYZ";
let name = buf.as_str_slice().slice(5, 10);
let bad = name.slice_unchecked(0, 15);       // past the end of `name`
```

`bad` reads `"alice;token"`. The read stays inside the backing array, so the
instance is memory-safe, but the application has leaked data.

Unconstrained is undefined behavior bounded by the sandbox. It takes its own
name because those four guarantees are what separate it from C's.

### Contract violation

A contract violation is a program reaching an operation outside the contract
the operation states. It is always a bug.

| Class         | Not a bug                                     | Contract violation                                                         |
| ------------- | --------------------------------------------- | -------------------------------------------------------------------------- |
| Unspecified   | initialization order, ambient calls, `ref_eq` | `get_unchecked` out of range                                               |
| Host-defined  | NaN bits, relaxed SIMD, `InsecureSeed`        | none                                                                       |
| Unconstrained | none                                          | `char::from_u32_unchecked` on a non-scalar, `slice_unchecked` off the view |

Two rules follow.

- Unconstrained behavior arises only from a contract violation. No operation
  used within its contract is unconstrained, so the language never holds a
  legitimate undefined behavior.
- Any build may detect a contract violation and trap. The test world may keep
  the checks an `_unchecked` function elides, for example. A build must not
  trap on unspecified or host-defined behavior that is not a contract
  violation.

### Writing it in the specification

A sentence names the class, and the term where it applies:

- "The order between independent initializers is _unspecified_."
- "Calling `get_unchecked` with an index out of range is a _contract
  violation_, and the result is _unspecified_."
- "Calling `char::from_u32_unchecked` on a value that is not a Unicode scalar
  is a _contract violation_, and the behavior is _unconstrained_."

### Classifying an `_unchecked` function

The suffix does not decide the class. What decides it is whether a violation
breaks a type's invariant.

- A violation that breaks none is unspecified. `get_unchecked` out of range
  returns an element left in the backing array, which is a valid `T`.
- A violation that breaks one is unconstrained: a `char` that is not a scalar
  value, a `String` that is not UTF-8, a `StrSlice` whose ends are not
  character boundaries or leave its view.

Each `_unchecked` function states its own class.

## Alternatives

- Undefined behavior. It permits anything, and the four guarantees above are
  exactly what Wado can promise beyond it.
- Implementation-defined, C's sense. Its requirement to document the choice
  means nothing with one implementation. Unspecified takes its place.
- Erroneous behavior, C++26's term. It already means something narrower: a
  defined result in an erroneous program. Contract violation says what Wado
  needs without colliding with it.
- Rust's "logic error". It names the mistake, not the behavior, so a sentence
  cannot say a behavior "is" one.
- Separate class names for buggy and non-buggy unspecified behavior. That
  folds two independent axes into one list. The term contract violation keeps
  them apart.

## Roadmap

- [ ] [Specification Overview](./spec-overview.md) defines the three classes
  and the term, and every site below links to it.
- [ ] [Expressions](./spec-expressions.md) states that the order between
  independent initializers is unspecified.
- [ ] The existing sites take the terms: the ambient call, `ref_eq`, the trap
  message, and relaxed SIMD (from "implementation-defined" to unspecified
  or host-defined), and NaN bits and `InsecureSeed`.
- [ ] Each `_unchecked` function in `core:prelude` states its class.

## Known gaps

- No build detects a contract violation yet. The rule permits it and nothing
  does it.
