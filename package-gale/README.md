# Gale — Grammar Adaptive LL Engine

Gale is a parser generator written in Wado that takes ANTLR4 `.g4` grammar files as input and outputs self-contained Wado parser source files.

## Design Motivation

Existing parser generators couple grammar and runtime in ways that cause maintenance friction:

- **Bison/yacc**: action code embedded in grammar files makes them host-language-specific
- **ANTLR4**: the generator and runtime must be kept at the same version; `antlr4-runtime` version drift is a persistent burden
- **PEG/packrat**: no lexer/parser separation; whitespace handling is awkward

Gale's answer: generate a **single self-contained `.wado` file** that inlines the runtime. No external dependency, no version drift. Each generated file carries the exact runtime snapshot used at generation time.

See [WEP: Gale — Grammar Adaptive LL Engine](../docs/wep-2026-03-02-gale.md) for full design rationale.

## Related WEPs

- [WEP: Gale](../docs/wep-2026-03-02-gale.md) — full design and architecture
- [WEP: Compile-Time File Inclusion](../docs/wep-2026-03-02-include-str.md) — `#include_str` used to inline the runtime

## Usage

```sh
# Generate parser from grammar (outputs to stdout)
wado run src/main.wado gen grammar.g4

# Write to file
wado run src/main.wado gen grammar.g4 --output grammar_parser.wado

# Inspect grammar IR (debugging)
wado run src/main.wado dump grammar.g4

# Inspect .g4 token stream
wado run src/main.wado dump grammar.g4 --tokens
```

## Project Structure

```
package-gale/
  wado.toml              — package manifest
  tests/
    grammars/            — real-world .g4 input grammars for integration/golden tests
    golden/              — expected generated output (golden fixtures)
  src/
    main.wado            — CLI entry point
    main_test.wado       — smoke test for the public API
    ir.wado              — GrammarIR: typed grammar representation
    ir_test.wado
    runtime.wado         — Span, Token, ParseError (inlined into generated files)
    runtime_test.wado
    generator.wado       — GrammarIR → .wado source (inlines runtime via #include_str)
    generator_test.wado  — includes golden tests
    g4/
      token.wado         — G4Token enum and helpers
      lexer.wado         — tokenize .g4 source text
      lexer_test.wado
      parser.wado        — recursive descent parser: tokens → GrammarIR
      parser_test.wado
```

## Implementation Status

| Phase | Description | Status |
| ----- | ----------- | ------ |
| 0 | Project scaffold, runtime types | Done |
| 1 | G4 lexer and parser | Done |
| 2 | Code generation (CST types, visitor, walk functions) | In progress |
| 2 | Lexer function generation (`fn tokenize`) | Planned |
| 2 | Recursive descent parser function generation | Planned |
