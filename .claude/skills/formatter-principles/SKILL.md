---
name: formatter-principles
description: Formatter Development Principles
---

# Formatter Development Principles

This document codifies the formatting rules for `wado-compiler/src/unparse.rs`.

## General

- **Line width limit**: `MAX_LINE_WIDTH = 120` characters.
- The formatter is **rule-based**: it does not preserve the original source's whitespace, blank lines between items, or semicolons. It reconstructs them from rules.
- The snapshot/rollback mechanism (`self.snapshot()` / `self.rollback(snap)`) is used to try inline formatting, then fall back to multiline.

## Semicolons

1. Every expression statement (`ExprStmt`) ends with `;`.
2. **Exception**: when a block is folded to a single line (via `try_unparse_block_inline`), the lone statement's trailing `;` is omitted: `{ expr }` not `{ expr; }`.
3. `match` expressions used as statements (`Stmt::Expr` wrapping `Expr::Match`) do **not** get a trailing `;` — they are control-flow-like, same as `if`/`while`/`loop`/`for` which are separate `Stmt` variants.

## Blank Lines Between Items

- Blank lines between top-level items, struct fields, enum cases, variant cases, flags members, and impl methods are **preserved from source** up to a maximum of 2 blank lines.
- Implementation: `emit_blank_lines_to(target_line)` consults `CommentMap::blank_lines_between` (caps at 2) and emits `\n` characters accordingly.
- `last_source_line` is updated after each item/element so the next call has the correct reference.

## Inline Block Folding

A block is eligible for single-line rendering (`try_unparse_block_inline`) if:

1. It contains **exactly one** `Stmt::Expr` statement.
2. The expression is **inline-safe** (not `Block`, `If`, `Match`, `Closure`, or `LabeledBlock`).
3. There are **no comments** inside the block span.
4. The rendered result `{ expr }` does **not** contain a newline and does **not** exceed `MAX_LINE_WIDTH`.

When these conditions are met, the block is rendered as `{ expr }` on the same line with no trailing semicolon.

Currently `try_unparse_block_inline` is called from:

- `unparse_if_stmt`: only when there is no `else` and no `init` binding. Falls back to multiline if the inline attempt wraps or exceeds width.

## Match Expressions

- **2 or fewer arms**: inline format attempted first (`match expr { P1 => e1, P2 => e2 }`), then falls back to multiline if it exceeds width or contains comments.
- **3 or more arms**: always formatted multiline, one arm per line:
  ```
  match expr {
      P1 => e1,
      P2 => e2,
      P3 => e3,
  }
  ```

## Tuple/List Literals (`[...]`)

Three formatting strategies are tried in order:

1. **Single-line** `[a, b, c]`: attempted first. Accepted if no internal newline and ≤ 120 chars.
2. **KV-list** (key-value): if all elements are 2-element tuple literals (`[[k1, v1], [k2, v2], ...]`) and there are ≥ 2 of them, each entry gets its own line with a trailing comma:
   ```
   [
       ["key1", val1],
       ["key2", val2],
   ]
   ```
   This matches the Wasm Component Model associative array convention.
3. **Fill-style**: elements are packed onto lines, wrapping to a new line when the next element would exceed `MAX_LINE_WIDTH`.

## Logical Operators (`&&` / `||`)

- Attempted inline first.
- If inline exceeds width or contains a newline, falls back to `unparse_logical_chain_multiline`, which places the operator at the start of each continuation line, indented.

## Struct/Call Arguments with Trailing Comma

- If a struct literal or call has a source-trailing comma (`has_trailing_comma`), it is formatted multiline (one argument/field per line).
- Without a trailing comma, single-line is tried first, then fill-style multiline.

## Comments

- Comments are attached to AST nodes via `CommentMap` (keyed by byte offset).
- Leading comments are emitted before items via `emit_leading_comments_for_item`.
- Inline comments (`//` after code on the same line) are emitted after the line's content.
- Comments inside a block prevent inline folding.

## Safety: Always Commit Before Formatting

**`mise run format-wado` is destructive.** The formatter is rule-based and discards information that is not encoded in the AST (e.g. semicolons inside inline blocks, specific whitespace). Running it on uncommitted changes can silently destroy work.

Rules:

1. **Always commit (or stash) before running `mise run format-wado`.**
2. After formatting, review the diff carefully.
3. If the formatter has dropped meaningful information (e.g. a comment, a blank line that was intentional, or changed semantics), **reset the commit** (`git reset HEAD~1`) and fix the formatter first.

## Adding New Formatting Rules

1. Write a failing test: add a dirty fixture to `wado-compiler/tests/format.fixtures/all-dirty.wado` with the messy form, and add the expected clean form to `wado-compiler/tests/generated/format.fixtures/all.clean.wado`.
2. Run `cargo test -p wado-compiler -- format` to confirm failure.
3. Implement the rule in `unparse.rs`.
4. Run `mise run update-golden-format-fixtures` to regenerate golden files.
5. Run `mise run format-wado` to apply the formatter to all Wado sources.
6. Run `cargo test -p wado-compiler` to verify.
