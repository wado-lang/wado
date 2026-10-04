---
name: debugger
description: Use rust-gdb to inspect variables and step through code without modifying it. Prefer it over print debugging for any investigation into what the compiler is actually doing (lldb is unavailable on Claude Code Web).
---

# Debugger

lldb cannot launch under Claude Code Web's ptrace restrictions; use rust-gdb.

## Build

`dev` optimizes the compiler and keeps only line tables, so `info locals` comes
back empty. The `debugger` profile has full DWARF, no optimization, and its own
`target/debugger/`:

```sh
cargo build --profile debugger --bin wado
```

## Run

```sh
cat > /tmp/gdb_commands.txt << 'EOF'
file ./target/debugger/wado
set pagination off
break <file.rs>:<line> if $_streq(<a String local>.data_ptr, "…")
commands
silent
bt 6
print <a local>
continue
end
run compile -o /tmp/out.wasm example/hello.wado
quit
EOF
rust-gdb --batch -x /tmp/gdb_commands.txt > /tmp/gdb.log 2>&1
grep -a '^\$[0-9]* = ' /tmp/gdb.log | sort -u
```

- Fill in a line and locals that exist there; gdb names a missing symbol and
  prints nothing else.
- Make the breakpoint select with a condition, so one run answers the question
  and `bt` names the origin.
- Print a Rust `String` with `print`; `printf "%s"` aborts the command file.
  Compare one with `$_streq(s->data_ptr, "lit")`.
- `--batch` stops at the first command error. Check the log for
  `Error in sourced command file` before trusting an empty result.

To find every place an invariant breaks, an `assert!` enumerates them in one
run; gdb is for the values behind one that fired.
