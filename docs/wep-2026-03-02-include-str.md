# Compile-Time File Inclusion (`#include_str`, `#include_bytes`)

## Context

The existing `#data` compile-time literal embeds text from the same source file's `__DATA__` section. Code generators and tools need to embed content from _external_ files at compile time — for example, a parser generator that inlines a runtime library into its output, or a tool that ships a bundled template.

Rust's `include_str!()` and `include_bytes!()` macros are the established pattern. Wado has no macros, but the `#` compile-time prefix provides a natural home for these.

## Decision

Introduce `#include_str("path")` and `#include_bytes("path")` as compile-time expressions that read a file and produce its content as a constant.

### Syntax

```wado
let runtime: String = #include_str("./runtime/runtime.wado");
let template = #include_str("../templates/header.html");

let icon: List<u8> = #include_bytes("./assets/logo.png");
let cert = #include_bytes("./certs/root.der");
```

The argument is a string literal (not a runtime expression). The path is resolved relative to the source file containing the expression — the same convention as `#file`.

Unlike the argument-free compile-time literals (`#file`, `#line`, `#function`, `#data`), `#include_str` and `#include_bytes` take a parenthesized string literal argument. The parser distinguishes argument-free from argument-taking forms by looking ahead for `(` after the identifier.

### Types

| Literal                  | Return type | Use case                                            |
| ------------------------ | ----------- | --------------------------------------------------- |
| `#include_str("path")`   | `String`    | Text files: source code, HTML, JSON, SQL            |
| `#include_bytes("path")` | `List<u8>`  | Binary files: images, certificates, compiled assets |

Values are constant and inlined at every use site.

### Error Cases

| Condition                          | Error                                                            |
| ---------------------------------- | ---------------------------------------------------------------- |
| File not found                     | Compile error: `file not found: "./path"`                        |
| Path is not a string literal       | Compile error: `#include_str requires a string literal argument` |
| File is not valid UTF-8 (str only) | Compile error: `file is not valid UTF-8: "./path"`               |

Self-inclusion (a file including its own path) is **not** an error. `#include_str` reads a file as text without any code expansion — there is no recursive expansion as in C's `#include`. A file including itself simply returns its own source as a string literal, which is a valid use case (e.g., a script that embeds its own source for `--help` output).

### Compile-Time Snapshot

The file is read once at compile time. If the file changes after compilation, the compiled output is unaffected. This matches the semantics of `#data` in Wado and `include_str!()` in Rust.

### Relationship to Other Compile-Time Literals

`#data`, `#include_str`, and `#include_bytes` are complementary:

| Literal                  | Source                              | Return type | Use case                                                   |
| ------------------------ | ----------------------------------- | ----------- | ---------------------------------------------------------- |
| `#data`                  | `__DATA__` section in the same file | `String`    | Inline test fixtures, config, schemas co-located with code |
| `#include_str("path")`   | External file (text)                | `String`    | Bundle runtime code, templates, external text assets       |
| `#include_bytes("path")` | External file (binary)              | `List<u8>`  | Bundle images, certificates, binary assets                 |

## Consequences

- Code generators can inline auxiliary files (runtimes, templates) into their output without shipping separate runtime packages (motivating use case: Gale parser generator inlining its runtime)
- File paths are captured in the compiler's dependency graph — changes to included files trigger recompilation
- `#ident` is argument-free; `#ident("arg")` takes a parenthesized string literal — visually consistent within the `#` family while remaining syntactically distinguishable
- No impact on Wado programs that do not use these literals
