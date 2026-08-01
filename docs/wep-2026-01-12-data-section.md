# WEP: Data Section (`__DATA__`)

Status: Implemented

## Context

A module often carries static text that belongs with it — configuration, fixtures, embedded documents, metadata a tool reads — and a companion file for it drifts from the source it describes.

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

- The marker must start at column 1 and be alone on its line — only a newline or EOF may follow it. Anywhere else, `__DATA__` is an ordinary identifier.
- Everything after the marker line, verbatim to EOF, is the content. The marker line is excluded and the content is not trimmed.
- The content is never tokenized, so it may hold text that is not valid Wado.
- A module has at most one data section, and `wado format` re-emits it unchanged.

### Accessing the Content

The `#data` compile-time literal expands to the module's data section as a `String`. Using it in a module with no data section is a compile error.

```wado
export fn run() with Stdout {
    println(#data);
}

__DATA__
Hello from the data section!
```

`#data` is one of the compile-time location literals; see [Compile-Time Location Literals](./wep-2026-01-23-compile-time-location-literals.md).

The literal yields raw text. There is no format-parsing form: a program that wants structured data parses the text itself (`core:json`, `core:cbor`, …), keeping the compiler free of format knowledge.

For tooling, the compiler exposes the content on the parsed module, so a tool can read it without compiling the file.

## Consequences

### Positive

- Self-contained: the data travels with the module that owns it
- Module-scoped: each file has an independent data section, unlike Ruby
- Explicit access: `#data` makes the dependency visible at the use site
- Tooling-friendly: readable as plain text, without compiling the file

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

Rejected: it introduces a declaration whose value comes from nowhere visible, and it needs its own rules for placement, type, and duplicates. `#data` is an expression that reads exactly where the data is used.

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
// key: value
// other: value
```

Rejected: the payload is bound to the comment syntax, so it cannot hold arbitrary text, and multi-line data is awkward to write and to read back.

### Separate Data Files

```
hello.wado
hello.data.json
```

Rejected: file proliferation, and the pair drifts apart.
