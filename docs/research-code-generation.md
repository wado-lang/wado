# Research: Code Generation Approaches

Survey of code generation library designs and techniques.
Background for improving the codegen experience in the Wado ecosystem (package-gale, wado-from-idl, and future tools).

## Motivation

Wado's ecosystem has two code generators today:

1. **package-gale** — ANTLR4 `.g4` → self-contained Wado parser (written in Wado)
2. **wado-from-idl** — WIT → Wado stdlib bindings (written in Rust)

Both use the same basic approach: **manual string appending with explicit indentation**.
This works but scales poorly — `lexer_gen.wado` and `parser_gen.wado` in package-gale are
representative of the problems:

### Pain Points in package-gale

**1. Pervasive `out.push_str()` noise.** Every line of generated code requires an explicit
`out.push_str(...)` call. The generator code bears little visual resemblance to the output:

```wado
// What the generator looks like (lexer_gen.wado:90-109)
out.push_str("struct Lexer {\n");
out.push_str("    chars: Array<char>,\n");
out.push_str("    pos: i32,\n");
out.push_str("}\n\n");
out.push_str("impl Lexer {\n");
out.push_str("    fn new(input: &String) -> Lexer {\n");
out.push_str("        return Lexer { chars: input.chars().collect(), pos: 0 };\n");
out.push_str("    }\n\n");
// ... 20 more lines of this
```

**2. Manual indentation threading.** Every function takes an `indent: &String` parameter and
manually computes inner indentation as `` `{indent}    ` ``. This is error-prone and clutters
every function signature:

```wado
fn gen_lexer_elem(
    out: &mut String,
    elem: &LexerElement,
    all_rules: &Array<LexerRule>,
    indent: &String,         // threaded everywhere
    fail_action: &String,
    ctr: &mut i32,
)
```

**3. Counter-based label generation.** Unique labels require a `ctr: &mut i32` threaded
through all functions, with manual `*ctr += 1` bookkeeping.

**4. Escaped braces in template strings.** Wado template strings use `{}` for interpolation,
but the generated Wado code also uses `{}` for blocks. Every brace in the output must be
escaped as `\{` and `\}`:

```wado
out.push_str(`{indent}if pos >= chars.len() \{ {fail_action}; \}\n`);
```

**5. Duplicated logic.** `dedup_name` / `get_count` / `increment_count` are implemented
twice — once in `parser_gen.wado` and once in `generator.wado` — because the
field-name-deduplication concern is spread across both modules.

**6. No structural guarantees.** Nothing prevents generating syntactically invalid Wado
(mismatched braces, missing semicolons). Bugs are only caught when compiling the generated
output.

### Pain Points in wado-from-idl

Less severe because the Rust `writeln!` macro is less noisy and `indent: usize` with
`"    ".repeat(self.indent)` is cleaner than string-concatenated indentation. But the same
fundamental issues apply: no structural guarantees, no import management, manual formatting.

### Requirements for a Better Approach

In priority order:

1. **Ergonomics** — The generator code should be easy to write and read. Ideally, it should
   visually resemble the output code.
2. **Performance** — Code generation speed matters for the compiler toolchain. String
   allocation should be minimized.
3. **Type safety** — Ideally, the API should make it hard to generate syntactically invalid
   code. But Wado has no lifetimes and simpler syntax than Rust, so this is a
   nice-to-have, not a hard requirement.

---

## Approach 1: Quasi-Quoting (quote, genco)

### How It Works

Write code that _looks like_ the target language, with interpolation points for dynamic
values. A macro transforms the quasi-quoted code into an internal representation at compile
time.

### Rust's `quote` (dtolnay)

The `quote!` macro produces `proc_macro2::TokenStream`:

```rust
let name = format_ident!("Lexer");
let tokens = quote! {
    struct #name {
        chars: Array<char>,
        pos: i32,
    }
};
```

- Interpolation via `#var`; repetition via `#(#items),*`
- Any `ToTokens` implementor can be interpolated
- Produces a flat token stream — **no formatting**. Must pipe through `prettyplease` or
  `rustfmt` for readable output
- Rust-only (generates Rust token streams)

### genco (udoprog)

Language-agnostic quasi-quoting with automatic import management:

```rust
use genco::prelude::*;

let tokens: rust::Tokens = quote! {
    struct Lexer {
        chars: Array<char>,
        pos: i32,
    }
};

let output = tokens.to_file_string()?;
```

- Supports 8 languages (Rust, Java, C#, Go, Dart, JavaScript, C, Python)
- Automatic import collection and deduplication
- Whitespace-aware: preserves the indentation structure from the quasi-quote
- `$var` interpolation syntax

### Assessment for Wado

| Aspect        | Rating     | Notes                                                                                                                                                                |
| ------------- | ---------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Ergonomics    | Excellent  | Generator code looks like the output                                                                                                                                 |
| Performance   | Good       | Compile-time macro expansion; runtime is string concatenation                                                                                                        |
| Type safety   | Structural | Prevents gross structural errors, not semantic ones                                                                                                                  |
| Applicability | Limited    | Both `quote` and `genco` are Rust proc macros — **cannot be used in Wado code** (package-gale is written in Wado). Could work for Rust-side tools like wado-from-idl |

**Key limitation:** Quasi-quoting requires a macro system. Wado intentionally has no macros.
A Wado quasi-quoting solution would need to be a language feature (unlikely) or a
compile-time code transformation tool (possible but heavy).

---

## Approach 2: Builder API (JavaPoet / KotlinPoet / codegen)

### How It Works

Typed builder objects mirror language constructs. Each builder produces a spec object;
rendering to string is a separate step.

### JavaPoet / KotlinPoet (Square)

The gold standard for builder-based codegen. Key design:

```java
// JavaPoet
TypeSpec lexer = TypeSpec.classBuilder("Lexer")
    .addField(FieldSpec.builder(TypeName.get(char[].class), "chars")
        .addModifiers(Modifier.PRIVATE)
        .build())
    .addMethod(MethodSpec.methodBuilder("atEnd")
        .returns(TypeName.BOOLEAN)
        .addStatement("return this.pos >= this.chars.length")
        .build())
    .build();
```

Core abstractions:

- **TypeSpec** — class, interface, enum, annotation
- **MethodSpec / FunSpec** — methods and functions
- **FieldSpec / PropertySpec** — fields and properties
- **ParameterSpec** — function parameters
- **CodeBlock** — method body content (string-based with format specifiers)

Format specifiers in CodeBlock:

- `$T` — type reference (auto-imports)
- `$N` — name reference (links to another spec)
- `$S` — string literal (auto-quoted)
- `$L` — raw literal

Control flow helpers:

```java
code.beginControlFlow("if (x > 0)")
    .addStatement("return x")
    .nextControlFlow("else")
    .addStatement("return -x")
    .endControlFlow();
```

**Hybrid design:** Declarations are type-safe builders; method bodies are string-based
`CodeBlock`s. This pragmatic split acknowledges that modeling every expression as a typed
node is impractical.

### codegen (carllerche, Rust)

Simpler builder API for Rust code:

```rust
let mut scope = Scope::new();
scope.new_struct("Lexer")
    .derive("Debug")
    .field("chars", "Vec<char>")
    .field("pos", "usize");
```

- Types are plain strings (no import management)
- Minimal formatting (recommends piping through `rustfmt`)
- Very simple to learn; good for straightforward struct/enum generation

### Assessment for Wado

| Aspect        | Rating   | Notes                                                             |
| ------------- | -------- | ----------------------------------------------------------------- |
| Ergonomics    | Good     | Builder chains are readable; method bodies are still string-based |
| Performance   | Good     | Minimal allocation if builders use pre-allocated buffers          |
| Type safety   | Partial  | Declarations are type-safe; bodies are not                        |
| Applicability | **High** | Can be implemented as a Wado library with no language changes     |

**Key advantage:** A builder API is just a library — it can be written in Wado and used by
package-gale without any compiler changes.

---

## Approach 3: AST Construction + Pretty-Printing

### How It Works

Define a typed AST representing the target language, build it programmatically, then
pretty-print it to source code.

### SwiftSyntax / SwiftSyntaxBuilder (Apple)

Two-layer API:

```swift
// Raw SyntaxFactory (extremely verbose)
let decl = StructDeclSyntax(
    name: .identifier("Lexer"),
    memberBlock: MemberBlockSyntax(
        members: MemberBlockItemListSyntax([...])
    )
)

// SwiftSyntaxBuilder (result builders, much better)
let source = SourceFileSyntax {
    StructDeclSyntax(name: "Lexer") {
        MemberBlockSyntax {
            // ...
        }
    }
}
```

- Source-accurate AST (preserves trivia/whitespace)
- Full compile-time type safety
- Raw API is extremely verbose (~2100 lines for a simple struct)
- Result builders reduce verbosity significantly

### Roslyn (Microsoft, C#)

```csharp
// SyntaxFactory (very verbose)
var structDecl = SyntaxFactory.StructDeclaration("Lexer")
    .WithMembers(SyntaxFactory.List(new[] {
        SyntaxFactory.FieldDeclaration(...)
    }));

// SyntaxGenerator (language-agnostic, less verbose)
var decl = generator.ClassDeclaration("Lexer",
    members: new[] { generator.FieldDeclaration(...) });
```

- Immutable syntax trees (modifications produce new trees)
- `NormalizeWhitespace()` for formatting
- The [Roslyn Quoter](https://roslynquoter.azurewebsites.net/) tool is practically
  required to find the right factory calls
- Separate APIs for C# and VB

### Pretty-Printing Algorithms

Two foundational approaches:

**Oppen (1980):** Imperative, buffer-based. Two interacting procedures (Scan + Print) with
bounded lookahead. Used by `rustc` and `prettyplease`. Optimal line-breaking decisions.

**Wadler (1997):** Functional, algebra-based. Defines a `Doc` datatype with combinators:

```
text("hello")                    -- literal text
line                             -- line break (or space when grouped)
nest(4, doc)                     -- indent by 4
group(doc)                       -- try to fit on one line
doc1 <> doc2                     -- concatenation
```

`group` is the key combinator: it tries to flatten the document onto one line; if it
doesn't fit, it breaks. This gives automatic line-wrapping behavior.

Wadler's approach is simpler to implement and reason about. Many formatters and
pretty-printing libraries are based on it (Paiges, various Haskell pretty-printers,
Prettier for JavaScript).

### Assessment for Wado

| Aspect        | Rating         | Notes                                                    |
| ------------- | -------------- | -------------------------------------------------------- |
| Ergonomics    | Poor to Medium | Raw AST construction is very verbose; builders/DSLs help |
| Performance   | Good           | Single-pass formatting; no string re-parsing             |
| Type safety   | Excellent      | Syntactically invalid code is unrepresentable            |
| Applicability | **High**       | Can be implemented as a Wado library                     |

**Key advantage:** Guarantees valid output. Decouples structure from formatting.

**Key disadvantage:** The generator code is verbose and looks nothing like the output.
Martin Fowler notes: "The real advantage in the AST lies when we want to read or write
multiple formats." If generating only Wado, the overhead may not be justified.

---

## Approach 4: Template-Based (Tera, Handlebars, Askama)

### How It Works

Write template files that look like the output, with placeholders and control flow
directives.

```
struct {{ name }} {
{% for field in fields %}
    {{ field.vis }} {{ field.name }}: {{ field.type }},
{% endfor %}
}
```

### Variants

| Library        | Compile-time?    | Performance   | Features                        |
| -------------- | ---------------- | ------------- | ------------------------------- |
| **Askama**     | Yes (Jinja-like) | ~330us/render | Type-checked at build time      |
| **Tera**       | No (Jinja2-like) | ~860us/render | Macros, inheritance, hot-reload |
| **Handlebars** | No (logic-less)  | ~3.7ms/render | Minimal logic, very stable      |

### Assessment for Wado

| Aspect        | Rating                   | Notes                                                                                                                                    |
| ------------- | ------------------------ | ---------------------------------------------------------------------------------------------------------------------------------------- |
| Ergonomics    | Excellent (simple cases) | Template looks like output                                                                                                               |
| Performance   | Good to Excellent        | Depends on compile-time vs runtime                                                                                                       |
| Type safety   | None                     | Can produce invalid code                                                                                                                 |
| Applicability | **Low**                  | Wado has no template engine; would need to build one. Templates also struggle with complex conditional generation (like LL(k) lookahead) |

**Key limitation:** Templates work well for repetitive, structurally uniform code.
Package-gale's generation logic is heavily conditional and recursive — it would fight
against a template approach.

---

## Approach 5: Indentation-Aware Writer (Practical Middle Ground)

### How It Works

A thin abstraction layer over string building that handles indentation automatically and
provides convenience methods for common patterns.

This is what wado-from-idl already does in Rust (`WadoCodeGenerator` with `indent: usize`
and `writeln()`), but it can be significantly improved.

### Design Sketch for Wado

```wado
struct CodeWriter {
    buf: String,
    indent: i32,
    fresh_line: bool,
}

impl CodeWriter {
    fn new() -> CodeWriter { ... }

    // Write text, auto-indenting at line starts
    fn write(&mut self, text: &String) { ... }

    // Write a full line (most common operation)
    fn line(&mut self, text: &String) { ... }

    // Blank line
    fn blank(&mut self) { ... }

    // Indent management
    fn indent(&mut self) { self.indent += 1; }
    fn dedent(&mut self) { self.indent -= 1; }

    // Block helper: emits opener, runs body indented, emits closer
    fn block(&mut self, opener: &String, f: fn() with stores[self]) { ... }

    // Unique ID generation (replaces manual counter threading)
    fn next_id(&mut self) -> i32 { ... }

    fn to_string(&self) -> String { ... }
}
```

Usage would look like:

```wado
fn gen_lexer_struct(w: &mut CodeWriter) {
    w.block("struct Lexer {", || {
        w.line("chars: Array<char>,");
        w.line("pos: i32,");
    });
    w.blank();
    w.block("impl Lexer {", || {
        w.block("fn new(input: &String) -> Lexer {", || {
            w.line("return Lexer { chars: input.chars().collect(), pos: 0 };");
        });
        // ...
    });
}
```

Compare with the current code (20 lines of `out.push_str()` with manual `\n` and spaces).

### Assessment for Wado

| Aspect        | Rating        | Notes                                                      |
| ------------- | ------------- | ---------------------------------------------------------- |
| Ergonomics    | Good          | Much cleaner than raw string appending                     |
| Performance   | Excellent     | Minimal overhead over raw string building                  |
| Type safety   | None          | Still string-based                                         |
| Applicability | **Excellent** | Pure library; zero language changes; incremental migration |

---

## Approach 6: Hybrid Builder + Writer (Recommended)

### How It Works

Combine a **builder API for declarations** (structs, variants, functions, globals) with an
**indentation-aware writer for imperative code** (function bodies, control flow).

This is the JavaPoet/KotlinPoet insight adapted for Wado: declarations have regular
structure that benefits from type-safe builders; function bodies have irregular structure
that fights against rigid AST modeling.

### Design Sketch

```wado
// Declaration builders
let s = WadoStruct::new("Lexer")
    .field("chars", "Array<char>")
    .field("pos", "i32");

// Emit declarations via builders
w.emit_struct(&s);

// Emit function with builder for signature, writer for body
w.emit_fn("new", [Param::new("input", "&String")], "Lexer", || {
    w.line("return Lexer { chars: input.chars().collect(), pos: 0 };");
});

// Complex control flow still uses the writer
w.block(`if pos >= chars.len() {`, || {
    w.line(`{fail_action};`);
});
```

### Assessment

| Aspect        | Rating        | Notes                                           |
| ------------- | ------------- | ----------------------------------------------- |
| Ergonomics    | Very Good     | Best of both worlds                             |
| Performance   | Excellent     | Builders are zero-cost wrappers over the writer |
| Type safety   | Partial       | Declarations are validated; bodies are strings  |
| Applicability | **Excellent** | Pure library; composable; incremental adoption  |

---

## Comparative Summary

| Approach                    | Ergonomics    | Perf      | Type Safety | Wado Feasibility       | Effort    |
| --------------------------- | ------------- | --------- | ----------- | ---------------------- | --------- |
| **Quasi-Quoting**           | Excellent     | Good      | Structural  | Impossible (no macros) | N/A       |
| **Full AST + Pretty-Print** | Poor          | Good      | Excellent   | Possible               | Very High |
| **Template Engine**         | Good (simple) | Good      | None        | Possible but poor fit  | High      |
| **Builder API**             | Good          | Good      | Partial     | Excellent              | Medium    |
| **Indent-Aware Writer**     | Good          | Excellent | None        | Excellent              | Low       |
| **Hybrid Builder + Writer** | Very Good     | Excellent | Partial     | Excellent              | Medium    |

---

## Decision: Indentation-Aware Writer (CodeWriter)

After evaluating all approaches against Wado's constraints, we chose the
**indentation-aware writer** as the primary approach:

- **package-gale is written in Wado** — Rust-only tools (quote, genco, prettyplease) are
  not applicable. The Wado ecosystem should not depend on Rust for code generation.
- **Wado has no macros** — quasi-quoting approaches are ruled out by language design.
- **Declaration builders add little value** for package-gale's current patterns. Declarations
  are generated linearly (all fields known upfront), so a writer with `begin`/`end` is
  equally readable. Builders can be added later if the writer proves insufficient.
- **No language extension needed** — the brace-escape problem is mostly solved by `begin`/`end`
  managing structural braces, with template strings reserved for lines with variable
  interpolation. The remaining edge case (inline braces + interpolation on the same line)
  is rare enough to tolerate `\{`/`\}` escaping.

### CodeWriter API Design

The writer lives in package-gale initially, and may be extracted to a shared library if
other Wado code generation tools emerge.

```wado
struct CodeWriter {
    buf: String,
    indent: i32,
    fresh_line: bool,
    next_id_counter: i32,
}

impl CodeWriter {
    fn new() -> CodeWriter { ... }

    // Core output
    fn write(&mut self, text: &String) { ... }   // write text, auto-indent at line starts
    fn line(&mut self, text: &String) { ... }     // write a full line
    fn blank(&mut self) { ... }                   // emit a blank line

    // Block structure — begin appends " {" and indents; end emits "}" and dedents.
    // This eliminates brace escaping for block-level braces (the 90% case).
    fn begin(&mut self, opener: &String) { ... }  // e.g. begin("struct Foo")
    fn end(&mut self) { ... }                     // emits "}"

    // Manual indent control (for cases that don't fit begin/end)
    fn indent(&mut self) { self.indent += 1; }
    fn dedent(&mut self) { self.indent -= 1; }

    // Unique ID generation (replaces manual counter threading)
    fn next_id(&mut self) -> i32 { ... }

    fn to_string(&self) -> String { ... }         // asserts indent == 0
}
```

### Before/After Comparison

Current package-gale code (manual string appending):

```wado
fn gen_lexer_struct(out: &mut String) {
    out.push_str("struct Lexer {\n");
    out.push_str("    chars: Array<char>,\n");
    out.push_str("    pos: i32,\n");
    out.push_str("}\n\n");
    out.push_str("impl Lexer {\n");
    out.push_str("    fn new(input: &String) -> Lexer {\n");
    out.push_str("        return Lexer { chars: input.chars().collect(), pos: 0 };\n");
    out.push_str("    }\n\n");
    // ...
}

fn gen_if_check(out: &mut String, indent: &String, ctr: &mut i32, ...) {
    let inner = `{indent}    `;
    *ctr += 1;
    out.push_str(`{indent}if pos >= chars.len() \{ {fail_action}; \}\n`);
}
```

With CodeWriter:

```wado
fn gen_lexer_struct(w: &mut CodeWriter) {
    w.begin("struct Lexer");
    w.line("chars: Array<char>,");
    w.line("pos: i32,");
    w.end();
    w.blank();
    w.begin("impl Lexer");
    w.begin("fn new(input: &String) -> Lexer");
    w.line("return Lexer { chars: input.chars().collect(), pos: 0 };");
    w.end();
    // ...
    w.end();
}

fn gen_if_check(w: &mut CodeWriter, ...) {
    let id = w.next_id();
    w.begin("if pos >= chars.len()");
    w.line(`{fail_action};`);
    w.end();
}
```

Key improvements:

1. **No `\n` or indent threading** — the writer handles both automatically
2. **No brace escaping** — `begin`/`end` manage structural braces
3. **No counter threading** — `next_id()` is on the writer
4. **Incremental migration** — functions can be migrated one at a time

### Future Extensions

If the declaration parts of package-gale remain noisy after migrating to CodeWriter,
**declaration builders** (JavaPoet-style) can be layered on top. These would compose with
the writer rather than replace it.

If the Wado ecosystem grows to need many code generation tools (formatter, LSP refactoring,
source transforms), a **Wadler-style pretty-printer** (`Doc` algebra with `text`, `line`,
`nest`, `group`) can be built as a separate library. This provides optimal line-wrapping
but is significantly more effort to implement.

### Not Recommended

- **Full typed AST:** Too verbose for the benefit. Wado's syntax is simple enough that
  string-based generation with good tooling is sufficient.
- **Template engine:** Poor fit for package-gale's deeply recursive, conditional generation
  logic. Would require building a template engine from scratch.
- **Quasi-quoting:** Requires macros, which Wado intentionally does not support.
- **Language extension for brace escaping:** The remaining edge cases are too rare to
  justify a new syntax (raw template strings, alternate interpolation delimiters, etc.).

---

## References

- [JavaPoet](https://github.com/square/javapoet) — Square's Java code generation library
- [KotlinPoet](https://square.github.io/kotlinpoet/) — Square's Kotlin code generation library
- [prettyplease](https://github.com/dtolnay/prettyplease) — Rust `syn` AST pretty-printer
- [quote](https://github.com/dtolnay/quote) — Rust quasi-quoting for proc macros
- [genco](https://github.com/udoprog/genco) — Language-agnostic code generation with import management
- [codegen](https://github.com/carllerche/codegen) — Simple Rust code generation
- [SwiftSyntax](https://github.com/apple/swift-syntax) — Apple's Swift syntax library
- [Roslyn SDK](https://learn.microsoft.com/en-us/dotnet/csharp/roslyn-sdk/) — Microsoft's .NET compiler platform
- [Wadler — A Prettier Printer (1997)](https://homepages.inf.ed.ac.uk/wadler/papers/prettier/prettier.pdf)
- [Oppen — Pretty Printing (1980)](https://doi.org/10.1145/357114.357115)
- [Martin Fowler — Generating Code for DSLs](https://martinfowler.com/articles/codeGenDsl.html)
- [Paiges](https://github.com/typelevel/paiges) — Wadler-style pretty-printing for Scala
