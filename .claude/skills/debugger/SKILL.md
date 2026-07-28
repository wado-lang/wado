---
name: debugger
description: Use rust-gdb to inspect variables and step through code without modifying it. Prefer this over print debugging when investigating compiler internals or runtime behavior (lldb is unavailable on Claude Code Web).
---

# Debugger

Debug wado compiler with rust-gdb.

## Build first

`dev` sets `debug = "line-tables-only"` and raises the workspace crates to
`opt-level = 1`, so `info locals` / `info args` come back empty in every
compiler frame. Build the `debugger` profile instead — full DWARF, no
optimization on the crates being stepped through, its own `target/debugger/`
dir so the `dev` cache stays warm:

```sh
cargo build --profile debugger --bin wado
```

## Usage

```sh
cat > /tmp/gdb_commands.txt << 'EOF'
file ./target/debugger/wado
set pagination off
break wado-compiler/src/codegen.rs:5985
run compile -o /tmp/out.wasm example/hello.wado
info locals
print *expr
bt 5
quit
EOF
rust-gdb --batch -x /tmp/gdb_commands.txt
```

## Common commands

| Command       | Description                   |
| ------------- | ----------------------------- |
| `info locals` | Show local variables          |
| `info args`   | Show function arguments       |
| `print *expr` | Dereference and print pointer |
| `bt 5`        | Backtrace (top 5 frames)      |
| `continue`    | Resume execution              |

## Notes

lldb does not work in Claude Code Web due to ptrace restrictions:

```
error: Cannot launch '...': personality get failed: Invalid argument
```

Use rust-gdb instead.
