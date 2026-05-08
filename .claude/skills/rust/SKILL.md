---
name: rust
description: Conventions for writing Rust code
---

- Manage dependencies in the workspace `Cargo.toml`.
- Use the `panic!` macro instead of falling back to meaningless dummy values or no-op implementations.
- Use the `2024` edition.
- Keep the code free of compiler warnings and Clippy lints.
- Avoid wildcard match arms (`_ => ...`) unless absolutely necessary.
