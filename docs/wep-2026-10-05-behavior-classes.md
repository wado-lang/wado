# WEP: Behavior Classes

## Context

The specification leaves some behavior open on purpose, and named it
inconsistently.

- A call to an `#[ambient]` function whose result goes unused may be removed,
  so its output may never appear.
- Globals initialize in dependency order, but the order between two
  independent initializers was not stated.
- `ref_eq` on references to two distinct places may answer either way.
- A trap's message was called "implementation-defined".
- Relaxed SIMD edge cases were called "implementation-defined but consistent
  within a single runtime".
- `get_unchecked` called out of range "yields an unspecified value of the
  element type or traps".
- Other `_unchecked` functions said only that they leave a check to the caller.

There was no word for what these share, and no word for how they differ. C's
"undefined behavior" is too strong for any of them. Every Wado program runs in
a Wasm sandbox, so even a broken `_unchecked` contract cannot corrupt the
engine or escape the component. It is not safe either. Inside the instance it
can return wrong answers or leak data.

## Decision

The specification names three behavior classes and one kind of program error.
[Behavior Classes](./spec-overview.md#behavior-classes) states them. A class
says what can happen. A contract violation says that reaching it is a bug. The
two are independent, and this section says why each is drawn where it is.

The terms are for the specification only. They add no syntax: there is no
`unsafe` block, and none is planned.

### Unspecified: the outcomes can be listed

After unspecified behavior, a reader can still follow the program by the
specification, case by case.

```wado
use { log_stderr } from "core:cli";

global A: i32 = logged("A", 1);
global B: i32 = logged("B", 2);

fn logged(name: String, value: i32) -> i32 {
    log_stderr(name);
    return value;
}
```

The log reads `A B` or `B A`. Either way `A == 1` and `B == 2` afterwards.

The compiler makes the choice, so a compiled program keeps it on every host. A
golden test of a compiled program can pin it.

### Host-defined: the host may choose

Host-defined behavior is unspecified behavior whose choice the compiled program
need not fix, so the host may make it. It is a class of its own because the
same compiled program can behave differently on wasmtime and on V8, so no golden
test can pin it.

The compiler may still fix the choice, and that is what lets it evaluate such
an operation at compile time. Before the class existed, a NaN's bits had no
stated latitude, so a constant fold never produced a NaN: the bits the
compiler would pick might not be the ones the engine would. With the bits
host-defined, any NaN is a permitted outcome, and a NaN-producing operation
folds like any other.

The cases differ in how consistent the host is, so each one says:

- A NaN's bit pattern may differ between two runs of one operation, as Wasm
  allows. The order on floats treats every NaN as one value, so only reading
  the bits observes it, as `to_bits` and `copysign` do.
- Relaxed SIMD results stay fixed on one host, as Wasm requires.
- `InsecureSeed` may return the same value every time. Its WIT says so.

### Unconstrained: the outcomes cannot be listed

Unconstrained behavior is undefined behavior bounded by the sandbox. It gets
its own name because the four guarantees the specification lists are what
separate it from C's.

The outcomes cannot be listed because a broken invariant travels. The optimizer
may assume the contract holds, and what follows from that reaches other
functions, later in time.

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
let bad = name.slice_unchecked(0, 11);       // past the end of `name`
```

`bad` reads `"alice;token"`. The read stays inside the backing array, so the
instance is memory-safe, but the application has leaked data.

### Contract violation: a second axis

Whether reaching a behavior is a bug is independent of its class:

| Class         | Not a bug                                     | Contract violation                                                         |
| ------------- | --------------------------------------------- | -------------------------------------------------------------------------- |
| Unspecified   | initialization order, ambient calls, `ref_eq` | `get_unchecked` out of range                                               |
| Host-defined  | NaN bits, relaxed SIMD, `InsecureSeed`        | none                                                                       |
| Unconstrained | none                                          | `char::from_u32_unchecked` on a non-scalar, `slice_unchecked` off the view |

The axis earns its term by what a build may do. A contract violation may be
detected and trapped, so the test world can keep the checks an `_unchecked`
function elides. Trapping on an initialization order that differs from what a
reader expected would be wrong.

Unconstrained behavior arises only from a contract violation. This keeps any
legitimate undefined behavior out of the language.

### Classifying an `_unchecked` function

The suffix does not decide the class. What decides it is whether a violation
breaks a type's invariant.

- A violation that breaks none is unspecified. `get_unchecked` out of range
  returns an element of the backing array, which is a valid `T`.
- A violation that breaks one is unconstrained: a `char` that is not a scalar
  value, a `String` that is not UTF-8, a `StrSlice` whose ends are not
  character boundaries or leave its view.

### Names not taken

- Undefined behavior. It permits anything, and the four guarantees are exactly
  what Wado promises beyond it.
- Implementation-defined, in C's sense. Its point is that the implementation
  documents its choice, which means nothing with one implementation.
  Unspecified takes its place.
- Erroneous behavior, C++26's term. It already means something narrower: a
  defined result in an erroneous program.
- Rust's "logic error". It names the mistake, not the behavior, so a sentence
  cannot say a behavior "is" one.
- Separate class names for buggy and non-buggy unspecified behavior. That folds
  two independent axes into one list.

## Roadmap

1. The specification defines the classes and the term, and the sites it
   already had take them: initialization order, the ambient call, `ref_eq`, the
   trap message, relaxed SIMD, NaN bits, what a world import returns,
   `get_unchecked` and `slice_unchecked`. Done.
2. Each `_unchecked` function in `core:prelude` states its class in its doc
   comment, under a `# Contract` heading rather than Rust's `# Safety`. Done.
3. Coverage holds a `char` to its values: a match over ranges on either side of
   the surrogate gap is exhaustive, so a `_` after them is unreachable. Gale's
   first-char dispatch emits no `_` when its arms already take every char.
   Done.
4. A float operation whose result is a NaN folds at compile time. Done:
   `const_eval` folds it to the canonical NaN, so the Wasm does not depend on
   the machine that ran the compiler. Only reading a stored f32 NaN declines:
   its bits are not host-defined, and widening it to the `f64` a `Value` holds
   may quiet it.

## Known gaps

- No build detects a contract violation yet. The specification permits it, and
  nothing does it.
