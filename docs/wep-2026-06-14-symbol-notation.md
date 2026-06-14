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
- The notation is purely textual; resolving it to an `AstId` and rendering an `AstId` back to it are follow-up work on `name.rs` and `wado query`.

## TODO

- [ ] Convert between this notation and the internal `name.rs` canonical form.
- [ ] Accept a symbol notation argument in `wado query` for name-based lookup.
- [ ] Emit this notation in diagnostics and `wado doc` anchors.
