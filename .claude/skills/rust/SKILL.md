---
name: rust
description: Workspace-specific Rust rules that override common habits: panic instead of dummy or no-op fallbacks, no wildcard match arms, dependencies managed in the workspace Cargo.toml, 2024 edition, zero warnings and clippy lints. Read before writing or editing Rust (.rs) code.
---

- Manage dependencies in the workspace `Cargo.toml`.
- Use the `panic!` macro instead of falling back to meaningless dummy values or no-op implementations.
- Use the `2024` edition.
- Keep the code free of compiler warnings and Clippy lints. CI runs `clippy -D warnings`, so any warning or lint fails CI.
- Avoid wildcard match arms (`_ => ...`) unless absolutely necessary.
