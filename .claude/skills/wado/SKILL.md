---
name: wado
description: Use when writing, editing, or reviewing Wado source code (.wado files). Provides the Wado language cheatsheet.
---

@../../../docs/cheatsheet.md

For the detailed specification of a specific feature, read the relevant WEP at `docs/wep-*.md`.

- Wado's import statements are similar to ES modules.
- Wado's tuple is similar to TypeScript.
- Wado's enum (without payload) and variants (with payloads; similar to Rust) can be used for `match` expressions & statements.
- Wado has value semantics & GC -- so no lifetime, no borrow, no .clone() methods unlike Rust
- Wado has string templates and format specifier built-ins with Display/DisplayAlt/Inspect/InspectAlt similar to Rust.
