---
name: mbt-wasip1-tools
description: Build small MoonBit WASIp1 CLI tools using the peter-jerry-ye/wasi library, focused on simple read/write tasks (echo, cat, wc, simple file ops). Use when creating or updating CLI examples, scripts, or skills for this repo.
---

# MoonBit WASIp1 Tools

## Use the examples

- Copy a project from `assets/` and modify it.
- Each example is a complete MoonBit project with a README.
- Build with `moon build -C <example-dir>` and run with `wasmtime run --dir host::guest <wasm>`.

## Path resolution (WASIp1)

- There is no cwd. All paths resolve against preopened directories.
- Resolution uses longest-prefix matching on preopen names.
- Relative paths must match a preopen prefix (for example, `foo/bar` requires a preopen named `foo`).
- Absolute paths only work when a preopen name starts with `/` (for example, `--dir /host::/`).
- If no prefix matches, fail immediately. Do not fall back to trial-and-error.

## Unified stdio

- `@wasi/stdio.stdin` implements `@sync_io.Reader` and `@io.Reader`.
- `@wasi/stdio.stdout` and `@wasi/stdio.stderr` implement `@sync_io.Writer` and `@io.Writer`.

## References

- `references/api.md` summarizes the core I/O and fs APIs.
