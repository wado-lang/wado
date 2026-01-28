---
name: debugger
description: Use rust-gdb to inspect variables without modifying code. Prefer this over print debugging when investigating compiler internals or runtime behavior.
---

# Debugger

Debug wado compiler with rust-gdb.

## Usage

```sh
cat > /tmp/gdb_commands.txt << 'EOF'
file ./target/debug/wado
set pagination off
break wado-compiler/src/codegen.rs:5985
run compile -o /tmp/out.wasm /tmp/test.wado
info locals
print *expr
bt 5
quit
EOF
rust-gdb --batch -x /tmp/gdb_commands.txt
```

## Common breakpoints

| Location                               | Description            |
| -------------------------------------- | ---------------------- |
| `wado-compiler/src/codegen.rs:843`     | `generate_wasm` entry  |
| `wado-compiler/src/codegen.rs:5970`    | `generate_expr` entry  |
| `wado-compiler/src/codegen.rs:9030`    | `generate_stmt` entry  |
| `wado-compiler/src/codegen.rs:9638`    | `generate_function`    |

## Common commands

| Command        | Description                     |
| -------------- | ------------------------------- |
| `info locals`  | Show local variables            |
| `print *expr`  | Dereference and print pointer   |
| `bt 5`         | Backtrace (top 5 frames)        |
| `continue`     | Resume execution                |

## Notes

lldb does not work in Claude Code Web due to ptrace restrictions:

```
error: Cannot launch '...': personality get failed: Invalid argument
```

Use rust-gdb instead.
