---
name: rust
description: "Workspace-specific Rust rules that override common habits: panic instead of dummy or no-op fallbacks, no wildcard match arms, dependencies managed in the workspace Cargo.toml, 2024 edition, zero warnings and clippy lints. Read before writing or editing Rust (.rs) code."
---

- Dependencies live in the workspace `Cargo.toml`. Edition `2024`.
- Zero warnings: `mise run clippy` runs with `-D warnings`.
- `panic!` rather than a dummy value or a no-op.
- No wildcard match arm (`_ => ...`) unless unavoidable.
- Import every item you name, and alias a collision (`use crate::ast::Ty as AstTy;`) rather than spelling the path out.
