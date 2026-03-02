# Compile-Time File Inclusion (`#include_str`)

## Context

The existing `#data` compile-time literal embeds text from the same source file's `__DATA__` section. Code generators and tools need to embed content from *external* files at compile time — for example, a parser generator that inlines a runtime library into its output, or a tool that ships a bundled template.

Rust's `include_str!()` macro is the established pattern: the file path is resolved at compile time, the file's UTF-8 content becomes a string literal embedded in the binary. Wado has no macros, but the `#` compile-time prefix provides a natural home for this.

## Decision

Introduce `#include_str("path")` as a compile-time expression that reads a file and produces its content as a `String` constant.

### Syntax

```wado
let runtime: String = #include_str("./runtime/runtime.wado");
let template = #include_str("../templates/header.html");
```

The argument is a string literal (not a runtime expression). The path is resolved relative to the source file containing the `#include_str` expression — the same convention as `#file`.

### Type

`#include_str(...)` has type `String`. Like `#file`, the value is constant and inlined at every use site — there is no heap allocation at the use site (the string is a compile-time constant).

### Error Cases

| Condition | Error |
|-----------|-------|
| File not found | Compile error: `file not found: "./path"` |
| Path is not a string literal | Compile error: `#include_str requires a string literal argument` |
| File is not valid UTF-8 | Compile error: `file is not valid UTF-8: "./path"` |
| Circular inclusion | Compile error: `circular #include_str detected` (unusual — only possible if a `.wado` file includes itself) |

### Compile-Time Snapshot

The file is read once at compile time. If the file changes after compilation, the compiled output is unaffected. This is intentional — it mirrors the semantics of `include_str!()` in Rust and `#data` in Wado.

### No `#include_bytes`

Binary file inclusion is not included. The use cases for binary inclusion in Wado source files are served by the bundled library mechanism (`wado-bundled-*`) and by Wasm data segments. A future WEP can add `#include_bytes` if a compelling use case emerges.

### Relationship to `#data`

`#data` and `#include_str` are complementary:

| Literal | Source | Use case |
|---------|--------|----------|
| `#data` | `__DATA__` section in the same file | Inline test fixtures, config, schemas |
| `#include_str("path")` | External file | Bundle runtime code, templates, external assets |

Both return `String`. Unlike `#data`, `#include_str` never requires a `__DATA__` section in the current file.

## Consequences

- Code generators can inline auxiliary files (runtimes, templates) into their output without shipping separate runtime packages
- File path is captured in the compiler's dependency graph — changes to included files trigger recompilation
- `#` prefix clearly signals compile-time evaluation; the argument form `(...)` distinguishes it from argument-free literals like `#file` and `#line`
- No impact on Wado programs that do not use `#include_str`
