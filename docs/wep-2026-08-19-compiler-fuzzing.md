# WEP: Compiler Fuzzing

## Context

The campaign exists to find `wado-compiler` bugs — wrong code above all, crashes
as well.

Two axes are independent: where inputs come from, and what decides that an input
found a bug. Most fuzzing supplies inputs and has only "it crashed" for an
oracle, which is the weaker half of what a compiler needs: a compiler's worst
bugs are silent, and a program that miscompiles still exits zero.

The position this WEP records is deliberate about which axis is scarce. The
oracle is strong; the input source is a fixed corpus.

## Decision

### The oracle is metamorphic

`builtin::black_box(false)` is a condition no pass can decide: the NIR optimizer
treats the call as opaque and `wir_build` emits the argument where the call
stood. A block behind such a guard is visible to every pass, unreachable at run
time, and absent from the emitted Wasm. Injecting one into a working program
must therefore leave that program's output untouched, and a difference is a
wrong-code bug.

There is no second compiler to compare against, and the fixture's recorded
expectation is not the oracle either — the program before injection is. That is
what lets the campaign judge a program it was never told the answer for, and it
is why the oracle transfers unchanged to inputs from any source.

### Inputs are transformed, not generated

The corpus is the e2e fixture set. Every mutant is an existing program plus
guards; nothing invents a program. Compiler coverage is therefore bounded by
what the fixtures happen to exercise, which is the known cost of this choice.
The roadmap states the condition for lifting it.

### Reach is widened by guard shape and payload shape

These are the two knobs, and they are ordered by the analysis family each
attacks rather than by novelty.

Guard shape decides what the surrounding control flow looks like: a branch (`if`)
or a loop (`while`), the latter aimed at the loop passes.

Payload decides what the dead region does to the live program:

- An opaque write to every writable binding in scope makes the dead region
  mutate live state, which is what the alias and mod/ref analyses behind `licm`,
  `store_load_forward`, `field_scalarize`, `copy_prop` and `sroa` rest on. It
  needs a writable binding, so it reaches only part of the corpus.
- An opaque read of every binding in scope makes live values appear used. It
  demands no mutability, so it reaches every site, and it attacks liveness, DCE,
  escape analysis and value-copy elision instead.
- Statements harvested from elsewhere in the same function reach the rest, at
  the cost of a free-variable analysis to keep them type-correct.

Each payload is its own subject, injected alone against the same baseline: a
fixture the compiler refuses one payload at stays in the campaign under the
others.

### Calibration precedes mutation

A fixture must be a valid oracle before it can be a subject. Calibration injects
an empty guard everywhere and keeps the fixtures whose observable behaviour is
unmoved; one that moves observes something an injection perturbs — a column, an
allocation address, a generated test-export name — and is recorded with that
reason rather than silently dropped.

A fixture whose own output moves between runs of the same program is a separate
case, and conflating it with a guard's doing is how a campaign learns to cry
wolf. A divergence re-runs the baseline first, and a baseline that disagrees
with itself is recorded as nondeterministic.

### A finding is reduced before it is reported

Injecting one site per compile is tens of thousands of compile-and-run cycles,
which neither CI nor a laptop carries. A mutant therefore carries every site at
once, and a divergence or a crash is delta-debugged back to the guards that
cause it, with the reduced mutant written out as source that can be read and
re-run. Without this a finding names a file; with it, a statement.

### Injection sites are structural

A site is a statement boundary inside a function or `test` body, and it must
start a token — a position inside a string literal parses after a guard is
spliced in, so the parse check alone cannot see it. Guards are written on a
single line, so code after an injection keeps the line numbers it had and a
fixture that prints an assertion diagnostic is not disturbed.

### CI is a campaign, not a gate

Schedule and dispatch only, never a PR trigger: the run takes far longer than a
review, and a finding is a bug report rather than a reason to block a change.
Work is sharded so every run covers the whole corpus, and findings fail the run.

## Roadmap

Ordered by yield per cost. Each run reports the corpus it drew and the sites
each payload reached.

- [x] Opaque read payload. Its first full run found three bugs: two colliding
      mangled names (`&&T` spelled as `&T`, a generic newtype spelled without
      its arguments) and a closure capture handed the box where the field
      holds the value.
- [ ] Retry a payload the compiler refuses without the bindings the error
      names. A resource binding whose read is a move costs a fixture the whole
      read payload.
- [ ] Recompile determinism as a second oracle: compile each fixture twice and
      compare the Wasm byte for byte. Catches what no output comparison can see.
- [ ] `while builtin::black_box(false) { … }` as a second guard shape.
- [ ] Calibrate and mutate at `O1`, `O2` and `Os` as well as `O0` and `O3`.
- [ ] Draw the corpus from `package-gale`, the stdlib and `example/` too. The
      runner assumes one file carrying a `__DATA__` spec, so this needs the
      harness widened first.
- [ ] Harvested-statement payload.
- [ ] Name the pass behind a finding by bisecting `WADO_LIST_PASSES` with
      `WADO_SKIP_PASS`, and write the reduced program out as a fixture.

### When to generate programs

A generator is the higher ceiling and is deferred, not rejected. The condition
for building one is that guard and payload shapes stop producing findings over a
full corpus run — until then the cheaper knob is still paying, and a generator
would be optimising the end that is not scarce.

The oracle transfers to generated programs unchanged. The work is the generator
itself: it must emit programs that type-check, terminate, are deterministic and
produce observable output, which is what the oracle requires of any subject.
Generating from the typed AST, so well-typedness holds by construction, is the
approach this design assumes.
