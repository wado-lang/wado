---
name: debugger
description: Debug Rust programs using rust-gdb. Use when investigating compiler internals, setting breakpoints, or inspecting variables during execution.
---

# Debugger

Debug Rust programs with rust-gdb.

## Installation

```sh
# rust-gdb is included with rustup
rustup component add rust-src  # optional: for source-level debugging
```

## Usage

### Batch mode (one-shot)

```sh
rust-gdb --batch \
  -ex "file ./target/debug/your_binary" \
  -ex "break path/to/file.rs:123" \
  -ex "run your_args" \
  -ex "info locals" \
  -ex "print *some_variable" \
  -ex "bt 5" \
  -ex "continue" \
  -ex "quit"
```

### Using a command file

```sh
cat > /tmp/gdb_commands.txt << 'EOF'
file ./target/debug/your_binary
set pagination off
break path/to/file.rs:123
run your_args
info locals
print *variable
bt 5
quit
EOF
rust-gdb --batch -x /tmp/gdb_commands.txt
```

### Interactive mode

```sh
rust-gdb ./target/debug/your_binary
(gdb) break src/codegen.rs:5985
(gdb) run compile -o out.wasm input.wado
(gdb) info locals
(gdb) print *expr
(gdb) bt
(gdb) continue
```

## Common commands

| Command | Description |
|---------|-------------|
| `break file.rs:123` | Set breakpoint at line |
| `run args` | Start program with arguments |
| `info locals` | Show local variables |
| `print *ptr` | Dereference and print pointer |
| `bt` / `bt 5` | Backtrace (full / top 5 frames) |
| `continue` | Resume execution |
| `next` | Step over |
| `step` | Step into |

## Notes

lldb does not work in Claude Code Web due to ptrace restrictions:

```
error: Cannot launch '...': personality get failed: Invalid argument
```

Use rust-gdb instead.
