# WEP: Symbol Notation

## Context

Wado needs one official, written way to name "this symbol in this module" — for docs, `wado query`, and diagnostics. The hard part is that a module reference is heterogeneous: a scheme (`core:json`), a relative path (`./utils.wado`), a remote URL (`https://x/lib.wado`, not yet implemented), or a bare dependency name (`parser-lib`). These already contain `:`, `/`, `.`, and `#`, so the module/symbol boundary must be unambiguous.

The compiler already has an _internal_ canonical name (`wado-compiler/src/name.rs`): module and symbol joined by `/`, members by `::`, trait impls by `^`. That `/` join is ambiguous against paths and URLs, so it is unfit as a user-facing notation.

## Decision

A symbol is written `MODULE # SYMBOL`.

- `MODULE` is the import specifier, exactly as it appears in `use … from "…"`.
- `#` separates module from symbol (matches Ruby/Javadoc/JSDoc and the URL-fragment intuition: "this anchor within the module").
- `SYMBOL` uses Wado's own surface operators, so the three symbol kinds are visible from the separator alone:
  - free symbol (function, global) → bare name
  - static-scoped (associated const/fn, static method, nested item) → `Type::name` (`::`, as in `f64::PI`)
  - method (instance) → `Type.name` (`.`, as in `point.x`)

Quoting: `MODULE` is quoted as in `import`, but **the quotes may be omitted when unambiguous** — i.e. for schemes and bare dependency names with no whitespace. Relative paths and URLs must be quoted, since they contain `#`, `/`, and `.`.

```
core:json#parse                          # free function (unquoted scheme)
core:math#f64::PI                        # associated constant
core:collections#TreeMap::new            # associated/static function
core:collections#TreeMap.insert          # instance method
core:collections#List<String>::len       # generics, Wado angle brackets
core:fmt#Point^Display::fmt              # trait-impl member (^ as internally)
"./utils.wado"#Helper::new               # relative path, quoted
"https://x/lib.wado"#foo                  # URL, quoted
```

Two registers, one grammar:

- Canonical form (machine: `wado query`, doc anchors) — always quote `MODULE`.
- Shorthand (prose) — drop quotes when unambiguous.

## Consequences

- `MODULE` reuses the import specifier verbatim, so any module the loader accepts is nameable with no new escaping rules.
- `::`/`.` match Wado source, so no second, conflicting operator vocabulary is introduced; the user's three kinds map to distinct separators.
- `#` is spent on the module boundary, so instance methods use `.` rather than Ruby's `Type#method`; Ruby users may pause briefly.
- The notation is purely textual. Resolving it to an `AstId` is implemented (`wado query`); the reverse — rendering an internal `name.rs` name back into this notation — remains follow-up.

## Implementation

The notation is a textual locator orthogonal to the position locator
(`--line`/`--column`): every `wado query` kind can, in principle, accept either.
Resolution loads the module named in the notation and looks the symbol up by
name, so no entry file is needed — relative modules anchor at `--base` (default
`.`), while `core:` / `wasi:` are location-independent.

- `wado_compiler::symbol_notation` — the parser (`MODULE#SYMBOL`).
- `Semantics::resolve_symbol_notation` — notation → `Definition`.
- `Engine::{definition,references,document_highlight,hover}_by_symbol` — drive
  resolution over a synthetic entry that `use`s the module.
- `wado query {definition,references,document-highlight,hover} --symbol
  <notation> [--base <dir>]`. `hover` also works position-based
  (`--line`/`--column`), rendering the signature / type.

Name-based results are bounded by what the analysis loads. `references` loads
the whole `--base` workspace by default (every `.wado` under it) so it spans
sibling files; `definition` / `hover` / `document-highlight` load only the
target module's import graph.

## TODO

- [x] Parse the notation (`wado_compiler::symbol_notation`).
- [x] `--symbol` locator for `definition` / `references` / `document-highlight`
      / `hover` on free / module-level symbols.
- [x] `hover` kind: notation (or position) → signature / type. The signature
      is the Wado definition with the body omitted, rendered by the same
      `unparse::*_signature` helpers that `wado doc` uses.
- [x] Two views via `wado query --all`: default = public-API (matches `wado
      doc`, private fields elided as `..`); `--all` = everything. A type's hover
      also lists its `impl` blocks (method / assoc-const signatures) as Wado.
- [x] `wado doc --all` mirrors the view, documenting private items/fields/
      inherent methods (default stays public).
- [x] Resolve members: `Type::m` / `Type.m` / `Type^Trait::m` (and generic
      types by base name) → methods and associated constants, by scanning the
      module's `impl` blocks and `trait` declarations. A member miss suggests
      the type's members.
- [x] `references` spans the whole workspace by default: the synthetic entry
      `use`s every `.wado` under `--base` (default cwd), so use→def edges from
      sibling files are recorded. The combined load is all-or-nothing, so if it
      fails the query re-imports only the files that analyze on their own,
      dropping any that can't load (e.g. a compile-time-codegen module without a
      build cache).
- [ ] Include doc-comment summaries in `hover` output.
- [ ] `wado doc` anchors / type cross-links keyed by the notation.
- [ ] Convert internal `name.rs` names back into this notation (so optimizer
      remarks / profiler / dumps can print queryable names).
