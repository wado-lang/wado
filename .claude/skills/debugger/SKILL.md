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

## Ask one question per run, not one per build

A breakpoint that fires thousands of times and gets `grep`ed answers one
question and costs a rebuild for the next. Make the breakpoint itself select:

```
break wado-compiler/src/wir_build/calls.rs:131 if $_streq(name->data_ptr, "…")
break …/func_inst.rs:2052
commands
silent
bt 6
continue
end
```

`bt` at a conditional hit gives the origin outright — the thing print
debugging cannot produce without guessing where to put the next `eprintln!`.

## Printing Rust values

`printf "%s", s` fails on a Rust `String` (it is a struct, not a `char*`) and
aborts the whole command file with `Value can't be converted to integer`. Use
`print`, which rust-gdb's pretty printers handle:

```
print fq              # "core:prelude/string.wado/String^Eq::eq"
print *expr
print info.struct_name
```

`$_streq(s->data_ptr, "lit")` is the way to compare one inside a breakpoint
condition.

## Batch runs

`rust-gdb --batch` exits on the first command error, so a typo in a `commands`
block silently truncates the rest of the run — check the tail of the output for
`Error in sourced command file` before trusting an empty result. Redirect to a
file and grep it; the DWO-loading noise otherwise buries the hits:

```sh
rust-gdb --batch -x /tmp/gdb_commands.txt > /tmp/gdb.log 2>&1
grep -a '^\$[0-9]* = ' /tmp/gdb.log | sort -u
```

## When a guard beats a breakpoint

The debugger answers "what is this value here". When the question is "where
else does this invariant break", an assertion at the point the invariant must
hold enumerates every violation in one run and keeps doing so afterwards —
the same reason a newtype that makes an illegal name unconstructible beats
chasing one miscompile at a time. Reach for `debug_assert!` / `assert!` in the
merge or registration step first, and for gdb once it fires and you need the
values behind it.

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
