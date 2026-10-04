---
name: rust
description: "Workspace-specific Rust rules that override common habits: panic instead of dummy or no-op fallbacks, no wildcard match arms, dependencies managed in the workspace Cargo.toml, 2024 edition, zero warnings and clippy lints. Read before writing or editing Rust (.rs) code."
---

- Manage dependencies in the workspace `Cargo.toml`.
- Use the `panic!` macro instead of falling back to meaningless dummy values or no-op implementations.
- Use the `2024` edition.
- Keep the code free of compiler warnings and Clippy lints. `mise run clippy` runs `-D warnings`, so any warning or lint fails it.
- Import every item you name. A `crate::` or `super::` path goes in a `use` item at the top of the module; where the item is read, write its name. `use crate::elaborator::sem::Ty;` then `Ty`, never `crate::elaborator::sem::Ty` inline. Alias a collision (`use crate::ast::Ty as AstTy;`) rather than spelling the path out. A hook refuses an edit that writes one inline, and `mise run check-rust-paths` gates the rest.
- Avoid wildcard match arms (`_ => ...`) unless absolutely necessary.
