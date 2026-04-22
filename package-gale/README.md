# Gale — Grammar Adaptive LL Engine

Gale is a parser generator written in Wado that takes ANTLR4 `.g4` grammar files as input and outputs self-contained Wado parser source files.

## Compatibility Principle

**Gale aims for full compatibility with the ANTLR4 `.g4` grammar syntax.** Any well-formed grammar accepted by the upstream `antlr4` tool should also be accepted by Gale's `parse()`.

The single exception is **action bodies**, which are intentionally skipped:

- `{ ... }` action blocks (rule-level, element-level, named actions like `@header`/`@members`/`@parser::name`)
- `{ ... }?` semantic predicates
- `catch [ ... ] { ... }` and `finally { ... }` bodies
- `@init { ... }` / `@after { ... }` rule prequel actions

These are recognized by the parser (so files containing them parse without error) but their bodies are discarded — they would have to be transpiled from the host language anyway, which is out of scope. Everything _around_ the action — its presence, position, and surrounding constructs — is preserved in the IR.

All other ANTLR4 features (`import`, `options`, `channels`, `tokens`, modes, `returns`/`throws`/`locals`, rule arguments, list labels `+=`, parser-side `~`/`.`, element options `<...>`, qualified IDs, lexer commands including `mode(X)`, etc.) are first-class and preserved.

## Design Motivation

Existing parser generators couple grammar and runtime in ways that cause maintenance friction:

- **Bison/yacc**: action code embedded in grammar files makes them host-language-specific
- **ANTLR4**: the generator and runtime must be kept at the same version; `antlr4-runtime` version drift is a persistent burden
- **PEG/packrat**: no lexer/parser separation; whitespace handling is awkward

Gale's answer: generate a **single self-contained `.wado` file** that inlines the runtime. No external dependency, no version drift. Each generated file carries the exact runtime snapshot used at generation time.

Target-language-dependent elements in `.g4` files (action blocks, semantic predicates, `superClass`, `@header`/`@members`) are warned and skipped, so real-world grammars work without manual cleanup.

See [WEP: Gale — Grammar Adaptive LL Engine](../docs/wep-2026-03-02-gale.md) for full design rationale.

## Usage

```sh
# Generate parser from grammar (outputs to stdout)
gale gen grammar.g4

# Write to file
gale gen grammar.g4 --output grammar_parser.wado

# Run via wado (development)
wado run package-gale/src/main.wado gen grammar.g4
```

## Project Structure

```
package-gale/
  wado.toml              — package manifest
  docs/
    antlr4-grammars.md   — ANTLR4 grammar format reference
  tests/
    grammars/            — .g4 input grammars for e2e/golden tests
    golden/              — expected generated output (golden fixtures)
  src/
    main.wado            — CLI entry point (gen subcommand)
    main_test.wado       — smoke test for the public API
    ir.wado              — GrammarIR: typed grammar representation
    ir_test.wado
    runtime.wado         — Span, Token, ParseError (inlined into generated files)
    runtime_test.wado
    codegen.wado         — GrammarIR → .wado source (inlines runtime via #include_str)
    codegen_test.wado    — golden tests against tests/golden/
    generator.wado       — Kiln-shape entry (stub; replaced by real impl in M6.7)
    lexer_gen.wado       — lexer function code generation
    parser_gen.wado      — parser function code generation
    gen_util.wado        — shared code generation utilities
    wadopoet.wado        — Wado source code builder (like JavaPoet)
    wadopoet_test.wado
    g4/
      token.wado         — G4Token enum and helpers
      lexer.wado         — tokenize .g4 source text
      lexer_test.wado
      parser.wado        — recursive descent parser: tokens → GrammarIR
      parser_test.wado
      integration_test.wado — parse real-world .g4 grammars
```

## Related WEPs

- [WEP: Gale](../docs/wep-2026-03-02-gale.md) — full design and architecture
- [WEP: Compile-Time File Inclusion](../docs/wep-2026-03-02-include-str.md) — `#include_str` used to inline the runtime
