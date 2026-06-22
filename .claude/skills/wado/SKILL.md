---
name: wado
description: Wado is not Rust: value semantics (deep copy on assign/pass), variant/enum/flags instead of one enum, an effect system, and no lifetimes/borrow-checker/unsafe/macros. Read this cheatsheet before writing, editing, or reviewing Wado (.wado) source — guessing the syntax from general knowledge will be wrong.
---

@../../../docs/cheatsheet.md

For the detailed specification of a specific feature, read the relevant WEP at `docs/wep-*.md`.

- Wado's import statements are similar to ES modules.
- Wado's tuple is similar to TypeScript.
- Wado's enum (without payload) and variants (with payloads; similar to Rust) can be used for `match` expressions & statements.
- Wado has value semantics & GC -- so no lifetime, no borrow, no .clone() methods unlike Rust
- Wado has string templates and format specifier built-ins with Display/DisplayAlt/Inspect/InspectAlt similar to Rust.
