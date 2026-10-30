---
name: rust
description: "Workspace-specific Rust rules that override common habits: panic instead of dummy or no-op fallbacks, never catch a panic outside tests, enums over unclear bool parameters, no wildcard match arms, dependencies managed in the workspace Cargo.toml, 2024 edition, zero warnings and clippy lints. Read before writing or editing Rust (.rs) code."
---

- Manage dependencies in the workspace `Cargo.toml`.
- Use the `panic!` macro instead of falling back to meaningless dummy values or no-op implementations.
- Never catch a panic in production code. A panic is a bug, and it ends the run. So there is no `catch_unwind`, a task is joined through `wado_host::sync::joined` rather than its `JoinError` turned into an error value, and a thread's panic is resumed where it is joined. Tests may catch one.
- Take an `enum` rather than a `bool` where the call site would not say what the flag means. `f(x, true)` makes the reader open `f`, and `f(x, Mode::Strict)` does not.
- Use the `2024` edition.
- Keep the code free of compiler warnings and Clippy lints. `mise run clippy` runs `-D warnings`, so any warning or lint fails it.
- Import every item you name. A `crate::` or `super::` path goes in a `use` item at the top of the module; where the item is read, write its name. `use crate::elaborator::sem::Ty;` then `Ty`, never `crate::elaborator::sem::Ty` inline. Alias a collision (`use crate::ast::Ty as AstTy;`) rather than spelling the path out. A `use` inside a function body counts as inline. A `pub(in …)` is exempt: it names a scope rather than reading an item, and Rust admits no import there. `mise run check-rust-paths` gates this against the empty baseline `scripts/rust-inline-paths.json`. Its detector (`package-gale/tools/rust_inline_paths.wado`) parses with the Gale Rust grammar, so the grammar decides what counts as a path. `mise run list-rust-paths <file.rs>…` lists what a file carries.
- Avoid wildcard match arms (`_ => ...`) unless absolutely necessary.
