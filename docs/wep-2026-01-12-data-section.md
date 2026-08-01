# WEP: Data Section (`__DATA__`)

Status: Implemented

## Context

E2E testing of the Wado compiler needs test metadata (expected stdout, stderr, exit code, target world) co-located with the test source. Programs also benefit from embedding static text (configuration, fixtures, documents) without a companion file.

Several languages provide similar mechanisms:

| Language | Keyword                | Access Method       | Scope                   |
| -------- | ---------------------- | ------------------- | ----------------------- |
| Perl     | `__DATA__` / `__END__` | `<DATA>` filehandle | Per-package             |
| Ruby     | `__END__`              | `DATA` IO object    | Global (main file only) |

The requirements:

1. Keep data co-located with source
2. Module-scoped, not global like Ruby
3. Explicit access, matching Wado's design philosophy
4. Readable by tooling without a full compilation

## Decision

### The `__DATA__` Marker

`__DATA__` ends the source code and starts the data section:

```wado
use { println } from "core:cli";

fn run() with Stdout {
    println("Hello, World!");
}

__DATA__
This is the data section.
It can contain any text.
```

Lexical rules:

- The marker must start at column 1 and be alone on its line — only a newline or EOF may follow it.
- Everything after the marker line, verbatim to EOF, is the content. The marker line itself is excluded; the content is not trimmed.
- The content is never tokenized, so it may hold any text, including text that is not valid Wado.
- A module has at most one data section.
- `__DATA__` that violates the placement rules (indented, or with trailing content on its line) is an ordinary identifier.

`wado format` round-trips the section: the marker and its content are re-emitted unchanged after the formatted source.

### Accessing the Content from Wado: `#data`

The `#data` compile-time literal expands to the module's data section as a `String` constant. Using it in a module with no data section is a compile error.

```wado
export fn run() with Stdout {
    println(#data);
}

__DATA__
Hello from the data section!
```

`#data` is one of the compile-time location literals; see [Compile-Time Location Literals](./wep-2026-01-23-compile-time-location-literals.md).

The literal yields raw text. There is no format-parsing form: a program that wants structured data parses the text itself (`core:json`, `core:cbor`, …), keeping the compiler free of format knowledge.

### Compiler API

`Module::data_section()` returns `Option<&str>` and is populated by the lexer, so it is available right after parsing, before type checking:

```rust
let parsed = wado_compiler::parse(source);
if let Some(data) = parsed.ast.data_section() {
    // Process the data section content
}
```

The content is carried through the pipeline: the TIR and NIR module nodes each expose `data_section()` and `with_data_section()`.

### E2E Test Format

Compiler E2E fixtures (`wado-compiler/tests/fixtures/*.wado`) put their test specification in the data section as strict JSON — `serde_json` parses it, so comments are not allowed.

```wado
export fn run() with Stdout {
    println("Hello");
}

__DATA__
{
  "stdout": "Hello\n"
}
```

The target world comes from the top-level key: no world key means `wasi:cli/command`, `"test": {}` selects the test world, and `"wasi:http/service": {...}` the HTTP world. A fixture with no data section at all defaults to the test world, so a library-shaped source doubles as a fixture verbatim.

The harness reads the section as text (`extract_data_section`) rather than compiling the fixture first, which keeps spec extraction independent of whether the fixture compiles — a fixture asserting `compile_error` still has a readable spec. The full field table lives in [`wado-compiler/CLAUDE.md`](../wado-compiler/CLAUDE.md).

## Consequences

### Positive

- Self-contained tests: expectations live with the test source
- Module-scoped: each file has an independent data section, unlike Ruby
- Explicit access: `#data` makes the dependency visible at the use site
- Tooling-friendly: available after parsing, and readable as plain text without parsing at all
- Strict JSON in fixtures: unambiguous, no dialect to implement

### Negative

- One unnamed section per file; multiple sections would need an extension such as `__DATA__:name`
- Text only — binary payloads need encoding (`#include_bytes` covers the binary case from a separate file)
- `__DATA__` is a top-level construct with no analogue elsewhere in the syntax

### Neutral

- Familiar to developers coming from Perl and Ruby
- `#data` shares its compile-time evaluation model with `#file`, `#line`, `#function`, and `#include_str`

## Alternatives Considered

### Magic Global Variable

```wado
println(DATA);  // Ruby-style
```

Rejected: implicit globals conflict with Wado's explicit philosophy.

### `#[data]` Attribute on a Binding

```wado
#[data]
let content: String;
```

Rejected: it introduces a declaration whose value comes from nowhere visible, and it needs its own rules for placement, type, and duplicates. `#data` is an expression that reads exactly where the data is used, and reuses the compile-time literal machinery.

### Compile-Time Format Parsing

```wado
#[data("json")]
let config: TreeMap<String, Any>;
```

Rejected: it puts format parsers and their diagnostics in the compiler, and pins the parsed shape to types the compiler must know. Parsing the raw text in Wado keeps formats in the standard library.

### Built-in Function

```wado
let content = builtin::data();
```

Rejected: less declarative, and its compile-time evaluation is less obvious than a `#`-prefixed literal.

### Comment-Based Directives

```wado
// CHECK-STDOUT: Hello
// CHECK-EXIT: 0
```

Rejected: less structured, and complex expectations are hard to express.

### Separate Expectation Files

```
tests/hello.wado
tests/hello.expected.json
```

Rejected: file proliferation, and the two drift apart.
