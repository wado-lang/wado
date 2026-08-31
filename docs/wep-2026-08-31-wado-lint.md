# WEP: `wado lint` — Corpus Checks

## Context

An agent writing Wado reinvents what already exists. The failure has two
shapes: proposing a design that was already refused, and writing a function
that already exists. This WEP addresses the second.

The compiler has no lint surface. Diagnostics today come from the elaborator —
type errors, and the five unused-item lints of
[Unused Diagnostics](./wep-2026-05-16-unused-diagnostics.md) — all computed per
module, before `reify`, and delivered through `CompilerHost` to both the CLI and
the LSP.

Duplicate detection does not fit that shape. It is a question about a corpus,
not about a module, and the functions worth matching against are precisely the
ones the program does not reference: a program that reinvents `core:temporal`
never imported `core:temporal`, so nothing loads it and no elaborate-time pass
can see it. Placing the check inside elaboration forces an index of the whole
standard library compiled into the binary — `wado-lsp` is I/O-free and builds
for `wasm32-unknown-unknown`, so it cannot read one from disk — plus a gate
against that index going stale, and a per-compile cost for a diagnostic that is
not useful per keystroke. A separate command pays none of it and can load
whatever corpus it needs.

Convention without mechanism does not hold. Almide's design philosophy states
"one name per operation, no aliases" and its cheatsheet lists `string.length` as
wrong; its own standard library defines `string.length` as an alias of
`string.len`, and `list.len` / `list.length` both resolve to one intrinsic.
Nothing checked the rule, so the rule leaked. See
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
save or from a code action later. The reverse order — embedding it in
elaboration and lifting it out afterwards — is not available.

### The first check: duplicate implementations

Two functions are reported when one is a reimplementation of the other. The
comparison runs on TIR, after names resolve and types are known: on the AST,
`a + b` and `a.add(b)` are different bodies, and `if !c { x } else { y }` and
`if c { y } else { x }` are different again.

Canonicalization of a body:

- Binders — parameters, locals, pattern bindings — become positional indices in
  order of first occurrence, so two bodies differing only in names agree.
- A callee reference becomes its `DeclId`, never its spelling
  ([Declaration Identity](./wep-2026-08-12-declaration-identity.md)).
- Types are part of the key, so `f(x: i32) -> i32 { x + 1 }` and
  `g(y: i64) -> i64 { y + 1 }` stay distinct.

### Matching a concrete function against a generic

The reinvention an agent actually writes is a generic instantiated by hand:

```wado
fn max_i32(a: i32, b: i32) -> i32 { if a > b { return a; } return b; }
pub fn max<T: Ord>(a: T, b: T) -> T   // core:prelude
```

Types in the key make these disagree, and so do the callee `DeclId`s: the
comparison in `max_i32` resolves to `i32`'s `Ord`, the one in `max` to the
method reached through the `T: Ord` bound. Matching them is a unification
question, not a hash lookup: does a substitution σ exist such that σ applied to
the generic body is the concrete body, with each bound-driven call in the
generic resolving to the impl selected at σ(T).

Two stages keep that affordable. A coarse key — the body's structural shape
alone, with every identifier and type erased — buckets candidates. Unification
runs only within a bucket.

### The standard library is index-side only

The stdlib is indexed and matched against; it never receives a finding. This
inverts the rule in Unused Diagnostics, where the stdlib is excluded from
reporting because it is not the user's code — here it is excluded as a target
for the same reason, while remaining the most valuable source.

### Exclusions

Not compared: synthesised functions (CM bindings, effect-dispatch wrappers,
monomorphisation clones, auto-derived impls), and two impls of the same trait
for different `Self`, where an identical body is ordinary. Two impls of
different traits with the same body are compared.

`#[allow(...)]` suppresses a finding on an item, and as a module inner
attribute on every item in a file, matching the mechanism Unused Diagnostics
already implements for `dead_code`.

By default the corpus is the current package plus the standard library.
Dependencies are indexed only behind a flag: a duplicate of a dependency's
function is not something the reader of the finding can fix.

### Near-duplicates

Exact structural equality misses the reimplementation that drifted. The second
check compares bodies by subtree fingerprint: hash every subtree above a
minimum size, sample the hashes by winnowing, and report pairs whose sampled
sets overlap beyond a threshold. Tree edit distance is not used — it is
quadratic over pairs, and the corpus is the whole standard library.

## Consequences

An agent that writes `fn strip_leading_spaces` is told `String::trim_start`
exists, which is the case this check is for. The finding names the existing
declaration and its location, and proposes the call that replaces it when the
signatures allow.

The check is not available while typing. It runs from the CLI, from CI, and
from the completion flow, and reaches the LSP only once the library is wired to
it. This is the intended trade: the check is worth running when a function is
finished, not on each keystroke, and a body mid-edit does not type-check and so
cannot be canonicalized at all.

`wado lint` is a new user-facing command and a new surface to keep. It is
introduced as a container rather than as a single check so that the second
check does not require a second command.

Elaborating the standard library on every run costs more than a compile that
imports three of its modules. The command is not on the inner development loop.

## Known gaps

Forwarders. A one-line function whose body is a call to another is the shape of
the alias this check was motivated by (`string.len` against `string.length`),
of every field getter, and of the internal-implementation-plus-public-facade
pattern that [Visibility](./wep-2026-06-25-visibility-internal-pub-export.md)
endorses through `pub use`. One rule has to separate them, and which rule is not
decidable from the shape alone. Closing it takes running the exact check over
`package-gale` and the standard library with no size floor, reading what the
forwarder-shaped matches actually are, and writing the rule from that.

The minimum body size. Below some node count every getter collides. The value
follows from the same measurement.

The near-duplicate threshold, and the minimum subtree size that feeds the
fingerprint. Both are unmeasurable before the exact check runs on a real corpus.
`package-gale` is the corpus to use: 61,342 lines of hand-written Wado ported
from ANTLR4, so structurally similar functions are dense in it, which is the
condition that makes a bad threshold visible.

Sharing the canonicalization with a function-merging pass. The same canonical
form identifies bodies that a NIR pass could merge after monomorphization, which
`wado-compiler/src/optimize/` has no pass for today. The policies are inverted —
this check excludes monomorphization clones, a merging pass targets them — so
only the canonicalization is shared, and the pass is not designed here.
