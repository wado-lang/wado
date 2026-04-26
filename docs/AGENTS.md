# Overview of Docs

This is the documentation directory of Wado.

## Rules for Markdown

- Keep documentation simple and MECE.
- Do not use `**...**` (bold) for sub-sections. Use markdown sections instead.
- Use markdown checklist for TODOs (`- [ ] ...`) and what's done (`- [x] ...`).
- After updating any docs, run `mise run format`.

## WEP: Wado Evolution Proposals

WEPs combine language specification and implementation strategy in a single document, covering both user-visible features and compiler architecture decisions.

Filename: `docs/wep-YYYY-MM-DD-{feature-name}.md`

- Title: Short description of the proposal
- Context: Background and problem statement
- Decision: What was decided and why
- Consequences: Impact and trade-offs

It may include TODOs on WIP.

### Index of WEPs

- [Target WASI P3 Only](./wep-2026-01-11-wasi-p3-only.md)
- [Deterministic Math Library (libm) Integration](./wep-2026-01-10-deterministic-libm.md)
- [Tagged Template Literals for Compile-Time Execution](./wep-2026-01-10-tagged-template-literals.md)
- [WebAssembly Module Import Support](./wep-2026-01-10-wasm-import.md)
- [Operator Precedence and Associativity](./wep-2026-01-11-operator-precedence.md)
- [Ambient Logging Functions](./wep-2026-01-12-ambient-logging.md)
- [Data Section (`__DATA__`)](./wep-2026-01-12-data-section.md)
- [Literal Type Conversion Rules](./wep-2026-01-12-literal-type-conversion.md)
- [Resource Lifecycle Management (RAII)](./wep-2026-01-12-resource-lifecycle.md)
- [Value Semantics and Reference Stores](./wep-2026-01-12-value-semantics-and-stores.md)
- [Struct and Trait System](./wep-2026-01-13-struct-and-trait.md)
- [Compiler Pipeline Refactoring](./wep-2026-01-14-compiler-pipeline-refactoring.md)
- [String Type Design](./wep-2026-01-15-string-type-design.md)
- [Tuple and Array Literal Syntax](./wep-2026-01-15-tuple-and-array-literals.md)
- [World Conformance and Export Syntax](./wep-2026-01-16-world-conformance-and-export.md)
- [Closure Implementation](./wep-2026-01-16-closure-implementation.md)
- [Function Return Type Syntax](./wep-2026-01-16-function-return-type-syntax.md)
- [CompilerHost Abstraction for Compiler I/O](./wep-2026-01-16-source-provider-abstraction.md)
- [Type Stringification](./wep-2026-01-16-type-stringification.md)
- [Template Format Specifiers](./wep-2026-01-17-template-format-specifiers.md)
- [JSON Literal Compatibility](./wep-2026-01-18-json-literal-compatibility.md)
- [JSON Module Import](./wep-2026-01-18-json-module-import.md) (superseded by Kiln)
- [Operator Overloading](./wep-2026-01-18-operator-overloading.md)
- [Iterator-Based Literal Coercion](./wep-2026-01-18-iterator-based-literal-coercion.md)
- [Effect System and Randomness in Collections](./wep-2026-01-20-effect-system-randomness.md)
- [Associated Types in Traits](./wep-2026-01-20-associated-types.md)
- [Indexing Traits Design](./wep-2026-01-20-indexing-traits.md)
- [String Template Desugaring](./wep-2026-01-20-string-template-desugaring.md)
- [Compile-Time Location Literals](./wep-2026-01-23-compile-time-location-literals.md)
- [Iterator Traits Design](./wep-2026-01-24-iterator-traits.md)
- [Module Loader Design](./wep-2026-01-24-module-loader.md)
- [Closure Parameter Monomorphization](./wep-2026-01-25-closure-parameter-monomorphization.md)
- [128-bit Integer Types (i128/u128)](./wep-2026-01-24-i128-u128-types.md)
- [Re-export Syntax (`pub use`)](./wep-2026-01-25-pub-use-reexport.md)
- [Variant Payload Design](./wep-2026-01-25-variant-payload-design.md)
- [Effect System Design](./wep-2026-01-27-effect-system-design.md)
- [Match Expression Design](./wep-2026-01-28-match-expression-design.md)
- [Global Variables](./wep-2026-01-27-global-variables.md)
- [WIT and Wado Mapping](./wep-2026-01-29-wit-wado-mapping.md)
- [Newtype Semantics](./wep-2026-01-29-newtype-semantics.md)
- [SIMD v128 Types](./wep-2026-01-31-simd-v128.md)
- [Format Traits](./wep-2026-02-01-format-traits.md)
- [Wasm Plan Phase](./wep-2026-02-03-wasm-plan-phase.md)
- [Trait Bounds Enforcement](./wep-2026-02-07-trait-bounds.md)
- [Variant Wasm GC Representation](./wep-2026-02-08-variant-representation.md)
- [Variant-Independent Types](./wep-2026-02-09-variant-independent-types.md)
- [Compile-Time Tuple Enumeration](./wep-2026-02-10-compile-time-tuple-enumeration.md)
- [Package Manifest (`wado.toml`)](./wep-2026-02-14-package-manifest.md)
- [Wasm IR (WIR) Layer](./wep-2026-02-14-wir-layer.md)
- [TIR-Level CM Binding Synthesis](./wep-2026-02-15-cm-binding-synthesis.md)
- [WASI HTTP Integration](./wep-2026-02-21-wasi-http.md)
- [Inspect (Debug Output)](./wep-2026-02-21-inspect-debug-output.md)
- [CLI Subcommands for Package Management](./wep-2026-02-22-cli-subcommands.md)
- [Struct Destructuring](./wep-2026-02-22-struct-destructuring.md)
- [Tuple Destructuring](./wep-2026-02-22-tuple-destructuring.md)
- [Base64 Encoding API](./wep-2026-02-27-base64-api.md)
- [Serialization and Deserialization (Serde)](./wep-2026-02-28-serde.md)
- [Documentation Generation (`wado doc`)](./wep-2026-02-28-doc-command.md)
- [Redesign Wasm CM Builtins as Resource Canonical Attributes](./wep-2026-03-01-cm-resource-canonical-attrs.md)
- [Compile-Time File Inclusion (`#include_str`)](./wep-2026-03-02-include-str.md)
- [Gale — Grammar Adaptive LL Engine](./wep-2026-03-02-gale.md)
- [Range Object](./wep-2026-03-03-range-object.md)
- [Default Trait](./wep-2026-03-04-default-trait.md)
- [Variadic Type Parameters](./wep-2026-03-14-variadic-type-parameters.md)
- [Conversion Traits (From, TryFrom, ? operator)](./wep-2026-03-16-conversion-traits.md)
- [WIT Bundling in Component Binaries](./wep-2026-03-21-wit-bundling.md)
- [Same-Scope Shadowing with Self-Reference](./wep-2026-03-25-same-scope-shadowing.md)
- [Migration to GC in Components](./wep-2026-03-28-gc-in-components.md)
- [Redesign String and Array APIs](./wep-2026-03-29-redesign-string-array-api.md)
- [WebIDL Binding Generator (`wado-from-idl`)](./wep-2026-04-01-tide.md)
- [Reactive Signals](./wep-2026-04-04-reactive-signals.md)
- [URL Standard Library (`core:url`)](./wep-2026-04-10-url-stdlib.md)
- [Default Arguments](./wep-2026-04-11-default-arguments.md)
- [Effect Handler](./wep-2026-04-11-effect-handler.md)
- [Kiln — Keyed IDL Lowering Notation](./wep-2026-04-12-kiln.md)
- [LSP Architecture](./wep-2026-04-18-lsp-architecture.md)
- [Generic `Subtask<T>` for CM async imports](./wep-2026-04-22-subtask-generic.md)
- [Synopsis Tests and Universal Test Discovery](./wep-2026-04-26-synopsis-tests.md)
