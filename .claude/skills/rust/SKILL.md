---
name: rust
description: Conventions for writing Rust in this workspace (2024 edition, workspace dependencies, panic-over-dummy, no wildcard match arms, clippy-clean). Use when writing or editing Rust (.rs) code.
---

- Manage dependencies in the workspace `Cargo.toml`.
- Use the `panic!` macro instead of falling back to meaningless dummy values or no-op implementations.
- Use the `2024` edition.
- Keep the code free of compiler warnings and Clippy lints.
- Avoid wildcard match arms (`_ => ...`) unless absolutely necessary.
