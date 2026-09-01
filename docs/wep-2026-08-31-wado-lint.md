# WEP: `wado lint` — Corpus Checks

## Context

An agent writing Wado rewrites things that already exist. That happens two ways:
proposing a design that was already refused, and writing a function that already
exists. This WEP is about the second.

The compiler has no lint surface today. Its diagnostics all come from the
elaborator — type errors, plus the unused-item lints of
[Unused Diagnostics](./wep-2026-05-16-unused-diagnostics.md). Those are computed
one module at a time, before `reify`, and delivered through `CompilerHost` to
both the CLI and the LSP.

Duplicate detection does not fit there, because it is a question about a whole
corpus rather than about one module. Worse, the functions most worth matching
against are exactly the ones the program never mentions. A program that rewrites
`core:temporal` by hand never imported `core:temporal`, so nothing loads it, so
no pass running during elaboration can see it.

Putting the check inside elaboration would mean compiling an index of the entire
standard library into the binary. The language service also builds for
`wasm32-unknown-unknown` to serve the browser playground, and there is no disk
there to read an index from. It would also need a gate to catch that index going
stale, and it would cost time on every compile for a diagnostic nobody needs on
every keystroke. A separate command pays none of that and can load whatever
corpus it wants.

There is also a reason not to settle for a written rule. Almide's design
philosophy says "one name per operation, no aliases", and its cheatsheet lists
`string.length` as wrong. Its own standard library defines `string.length` as an
alias of `string.len`, and `list.len` and `list.length` both resolve to a single
intrinsic. Nothing checked the rule, so the rule leaked. See
[the Almide survey](./research-language-survey-almide.md).

## Decision

### The command

`wado lint` is a container for corpus-level checks, not a single check. It
elaborates the target package plus the standard library, runs the enabled
checks over the result, and reports findings.

```sh
wado lint                          # the current package
wado lint --check duplicates       # one check
wado lint --format json            # machine-readable findings
```

Checks are named and individually selectable. A `[lint]` table in `wado.toml`
carries per-package configuration, alongside the existing `[format]` and
`[test]` tables: `exclude` for paths, and per-check enable and disable.
Findings exit non-zero.

The analysis is a library, separate from the command, so the LSP can call it on
save or from a code action later.

### The first check: duplicate implementations

Report two functions when one is a reimplementation of the other.

The comparison runs on TIR, after names are resolved and types are known. On the
AST it would not work: `a + b` and `a.add(b)` would look like different bodies,
and so would `if !c { x } else { y }` and `if c { y } else { x }`.

Before comparing, put each body into a canonical form:

- Replace every binder — parameters, locals, pattern bindings — with its
  position, numbered in order of first appearance. Two bodies that differ only
  in names then agree.
- Replace every callee reference with its `DefId`, never its spelling
  ([Declaration Identity](./wep-2026-08-12-declaration-identity.md)).
- Keep types in the key, so `f(x: i32) -> i32 { x + 1 }` and
  `g(y: i64) -> i64 { y + 1 }` stay separate.

### Matching a concrete function against a generic

What an agent actually writes is a generic function instantiated by hand:

```wado
fn sort_scores(xs: &mut List<i32>) { ... }   // written
impl<T: Ord> List<T> { pub fn sort(&mut self) { ... } }   // core:prelude/list.wado
```

These two do not match on the key. The types differ, and so do the callee
`DefId`s: the comparison in the hand-written body resolves to `i32`'s `Ord`,
while the one inside `sort` goes through the `T: Ord` bound.

Matching them is a unification problem rather than a hash lookup. The question
is whether some substitution σ turns the generic body into the concrete one,
with every bound-driven call in the generic resolving to the impl that σ(T)
selects.

Two stages keep the cost down. First a coarse key — the body's shape alone, with
every identifier and type erased — sorts candidates into buckets. Unification
then runs only inside a bucket.

### The standard library never receives a finding

It is indexed and compared against, but a duplicate is always reported on the
user's side of the pair. Unused Diagnostics leaves the stdlib out of reporting
for the same reason: it is not the reader's code to fix. This check keeps that
exclusion and adds the second role.

Dependencies work the same way when the flag below turns them on: indexed,
matched against, never reported.

### What is not compared

Synthesised functions are skipped: CM bindings, effect-dispatch wrappers,
monomorphisation clones, auto-derived impls.

Two impls of the same trait for different `Self` are skipped as well, because an
identical body there is normal. Two impls of _different_ traits with the same
body are compared.

`#[allow(...)]` silences a finding on one item, or on every item in a file when
written as a module inner attribute. This is the mechanism Unused Diagnostics
already implements for `dead_code`.

By default the corpus is the current package plus the standard library.
Dependencies are indexed only behind a flag, because a duplicate of a
dependency's function is not something the reader of the finding can fix.

### Near-duplicates

Exact structural equality misses a reimplementation that has since drifted. So
the second check compares bodies by subtree fingerprint: hash every subtree above
some minimum size, sample those hashes by winnowing, and report pairs whose
samples overlap past a threshold.

Tree edit distance is not used. It is quadratic over pairs, and the corpus here
is the entire standard library.

## Consequences

An agent that writes `fn strip_leading_spaces` gets told that
`String::trim_start` exists. The finding names the existing declaration and where
it lives, and where the signatures allow it, suggests the call that replaces the
new function.

The check is not available while you type. It runs from the CLI, from CI, and
from the completion flow. It reaches the LSP only once the library is wired up to
it. That costs little: a body being edited does not type-check yet, so it cannot
be canonicalized at all, and the only thing given up is checking a function that
is not finished.

`wado lint` is a new user-facing command and one more surface to maintain. It is
built as a container rather than as a single check so that adding a second check
later does not mean adding a second command.

Elaborating the standard library on every run costs more than compiling a program
that imports three of its modules. This command is not meant for the inner
development loop.

## Known gaps

Forwarders — what to do about a one-line function whose body is just a call to
another one. That shape covers three different things: the alias this check was
built for (`string.len` against `string.length`), every field getter, and the
internal-implementation-with-public-facade pattern that
[Visibility](./wep-2026-06-25-visibility-internal-pub-export.md) endorses through
`pub use`. One rule has to tell them apart, and which rule cannot be worked out
from the shape alone.

To close it: run the exact check over `package-gale` and the standard library
with no size floor, look at what the forwarder-shaped matches actually turn out
to be, and write the rule from that.

The minimum body size is the same problem. Below some node count every getter
collides with every other getter. The number comes out of the same measurement.

The near-duplicate threshold and the minimum subtree size are also unmeasurable
until the exact check has run on a real corpus. `package-gale` is the corpus to
use, because it is the largest body of hand-written Wado there is and it was
ported from ANTLR4, so structurally similar functions are dense in it. That
density is what makes a bad threshold visible.

Sharing the canonicalization with a function-merging pass. The same canonical
form would identify bodies that a NIR pass could merge after monomorphization,
and `wado-compiler/src/optimize/` has no such pass today. The two want opposite
policies, though: this check skips monomorphization clones and a merging pass
would target them. So only the canonicalization could be shared, and the pass is
not designed here.
