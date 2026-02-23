---
name: portable-safe-skills
description: Create portable, safe Codex skills focused on file or I/O operations under WASIp1 constraints. Use when authoring skills that must be portable across environments and avoid unsafe assumptions about paths, preopens, or stdio.
---

# Portable and Safe Skills

## Use skill-creator

- Follow the `skill-creator` workflow for skill structure, naming, and packaging.
- Keep SKILL.md short and move detailed content to `references/` or `assets/`.

## Required dependency

- Use `mbt-wasip1-tools` for concrete CLI patterns (echo, wc, read/write) and example project structure.

## Constraints to document

- WASIp1 path resolution uses longest-prefix matching against preopen names. No cwd. No fallback.
- Absolute paths only work if a preopen named `/` (or starting with `/`) exists.
- Stdio is unified: `@wasi/stdio` values implement both sync and async reader/writer traits.

## Minimal workflow

1. Start from an `assets/` example in `mbt-wasip1-tools`.
2. Keep logic simple and deterministic; only I/O is async.
3. Document the required `wasmtime run --dir host::guest` usage and expected preopens.
4. Add a README to each example with build/run commands.
5. Package the skill using the `skill-creator` guidelines when done.
