# WEP: Data Section (`__DATA__`)

**Date:** 2026-01-12
**Status:** Proposed

## Context

For E2E testing of the Wado compiler, we need a way to embed test metadata (expected stdout, stderr, exit code) within test fixture files. Rather than maintaining separate files for test inputs and expected outputs, a self-contained format is preferable.

Several languages provide similar mechanisms:

| Language | Keyword                | Access Method       | Scope                   |
| -------- | ---------------------- | ------------------- | ----------------------- |
| Perl     | `__DATA__` / `__END__` | `<DATA>` filehandle | Per-package             |
| Ruby     | `__END__`              | `DATA` IO object    | Global (main file only) |

Other testing approaches include:

- **Cram tests**: Shell session format with `$` prefixed commands
- **LLVM lit/FileCheck**: `RUN:` and `CHECK:` directives in comments
- **OCaml ppx_expect**: `[%expect {...}]` blocks with auto-promotion
- **Snapshot testing**: External golden files in `testdata/` or `__snapshots__/`

We want a solution that:

1. Keeps test data co-located with test source
2. Is module-scoped (not global like Ruby)
3. Aligns with Wado's explicit design philosophy
4. Supports structured data (JSON) for test metadata

## Decision

### The `__DATA__` Keyword

The `__DATA__` keyword marks the end of Wado source code and the beginning of a data section:

```wado
fn main() {
    println("Hello, World!");
}

__DATA__
This is the data section.
It can contain any text.
```

- `__DATA__` must appear at the start of a line (no leading whitespace)
- Everything after `__DATA__` until EOF is the data section content
- The `__DATA__` line itself is not included in the content
- Each module (file) can have its own data section

### The `#[data]` Attribute

To access the data section from within Wado code, use the `#[data]` attribute:

```wado
#[data]
let content: String;

fn main() {
    println(content);  // Prints the data section
}

__DATA__
Hello from the data section!
```

Rules:

- `#[data]` can only be applied to module-level `let` bindings
- The binding must have type `String`
- Compile error if `#[data]` is used without a `__DATA__` section
- Compile error if multiple `#[data]` bindings exist in a module

### Future Extension: Structured Data

In a future version, `#[data("format")]` will parse the data section:

```wado
#[data("json")]
let config: TreeMap<String, Any>;

#[data("jsonc")]  // JSON with comments
let test_spec: TestSpec;

__DATA__
{
  "exit": 0,
  "stdout": "Hello\n"
}
```

Supported formats (future):

- `"json"` - Standard JSON
- `"jsonc"` - JSON with `//` and `/* */` comments

Parse errors become compile errors, ensuring invalid test data is caught early.

### Compiler API for Tooling

The compiler exposes an API for tools (test runners, IDEs) to access data sections without full compilation:

```rust
// In wado-compiler
impl Module {
    /// Returns the data section content, if present.
    /// This is available after parsing, before type checking.
    pub fn data_section(&self) -> Option<&str>;
}
```

This allows test harnesses to:

1. Parse only the data section (fast)
2. Extract test expectations as JSON
3. Run the compiled program
4. Compare actual vs expected output

### E2E Test Format

For compiler E2E tests, the data section uses JSONC format:

```wado
// tests/fixtures/hello.wado
fn main() {
    println("Hello");
    eprintln("Warning");
}

__DATA__
{
  // Expected behavior
  "exit": 0,
  "stdout": "Hello\n",
  "stderr": "Warning\n"
}
```

Test specification schema:

```typescript
interface TestSpec {
  // Expected exit code (default: 0)
  exit?: number;

  // Expected stdout (exact match)
  stdout?: string;

  // Expected stderr (exact match)
  stderr?: string;

  // Alternative: pattern matching
  stdout_contains?: string[];
  stderr_contains?: string[];

  // Compiler behavior
  compile_error?: string; // Expected compilation failure
}
```

## Consequences

### Positive

- **Self-contained tests**: Test expectations live with test source
- **Module-scoped**: Each file has independent data section (unlike Ruby)
- **Explicit access**: `#[data]` attribute makes data usage visible
- **Early error detection**: JSON parse errors are compile-time errors
- **Tooling-friendly**: Compiler API enables fast test extraction
- **No YAML**: Uses JSON/JSONC which is strict and unambiguous

### Negative

- **Single data section per file**: Cannot have multiple named sections (could extend later with `__DATA__:name` if needed)
- **Text only initially**: Binary data requires encoding (base64) until future extensions
- **New syntax**: `__DATA__` is a new top-level construct to learn

### Neutral

- Similar to Perl/Ruby, so familiar to developers from those ecosystems
- The `#[data]` attribute pattern is consistent with other Wado attributes

## Implementation Notes

1. **Lexer**: Recognize `__DATA__` as end-of-source marker
2. **Parser**: Store data section content in AST's Module node
3. **Compiler API**: Expose `Module::data_section()` after parsing
4. **Codegen**: For `#[data]`, emit the string as a constant
5. **Test harness**: Parse data section as JSONC, run program, compare results

## Alternatives Considered

### Magic Global Variable

```wado
// Ruby-style
println(DATA);  // Magic global
```

Rejected: Implicit globals conflict with Wado's explicit philosophy.

### Built-in Function

```wado
let content = builtin::data();
```

Rejected: Less declarative; harder to optimize (compile-time evaluation less obvious).

### Comment-Based Directives

```wado
// CHECK-STDOUT: Hello
// CHECK-EXIT: 0
fn main() { println("Hello"); }
```

Rejected: Less structured; harder to parse complex expectations; LLVM-style is powerful but complex.

### Separate Expectation Files

```
tests/hello.wado
tests/hello.expected.json
```

Rejected: File proliferation; harder to keep in sync; less self-contained.
