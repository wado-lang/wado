# WEP: Function Return Type Syntax
**Status:** Accepted
**Date:** 2026-01-16

## Context

Wado needed to decide on the syntax for specifying function return types. Two primary options were considered:

1. **Arrow syntax (`->`)** - Used by Rust, Swift, Haskell, Python, and C++ (trailing return type)
   ```wado
   fn add(a: i32, b: i32) -> i32 { ... }
   ```

2. **Colon syntax (`:`)** - Used by TypeScript, Kotlin, and other JavaScript-adjacent languages
   ```wado
   fn add(a: i32, b: i32): i32 { ... }
   ```

This decision impacts:
- Developer experience and learning curve
- Consistency with closures and higher-order functions
- Alignment with target ecosystems (WebAssembly/systems vs. web development)
- Language readability, especially with complex type signatures

### Comparative Analysis

**Arrow Syntax (`->`) Advantages:**
- Provides visual separation between parameters and return types, improving readability for higher-order functions
- Maintains consistency between function type signatures and function definitions
- Aligns with mathematical notation `f: X → Y`
- Used by Rust and other systems programming languages in the WebAssembly ecosystem
- Clear directional semantics: "transforms input into output"

**Colon Syntax (`:`) Advantages:**
- Consistent with all type annotations using the same symbol
- Familiar to TypeScript/JavaScript developers
- Simpler syntax for straightforward functions

### Key Decision Factor: Higher-Order Functions

Wado's design includes:
- Effect systems (e.g., `fn foo() with Stdout, FileSystem`)
- Higher-order functions with complex type signatures
- Generic functions and methods

Example comparison with higher-order functions:

```wado
// Arrow syntax (clearer separation)
fn apply(f: fn(i32) -> i32, x: i32) -> i32 with Stdout

// Hypothetical colon syntax (more ambiguous)
fn apply(f: fn(i32): i32, x: i32): i32 with Stdout
```

The arrow syntax provides better visual parsing when function types and return types appear in the same signature.

### Research Findings

From community discussions:

**Rust Internals:** The `->` syntax was chosen to make the language "easier to parse for humans, especially in the face of higher-order functions." It originates from functional languages (Haskell, ML, OCaml) and serves as a visual "case marker" separating different parts of the function signature.

**Kotlin Community:** A proposal to change from `:` to `->` was rejected due to parsing complexity in higher-order functions (e.g., `fun bla(arg:(Int)->String):(String)->Int` becomes difficult to parse).

**C++ Design:** Trailing return type syntax (`auto f() -> T`) was introduced specifically to solve scoping issues with dependent return types and to support left-to-right reading order.

## Decision

Wado adopts **arrow syntax (`->`)** for function return types:

```wado
fn function_name(param: Type) -> ReturnType { ... }
```

This applies to:
- Regular function definitions
- Public functions
- Functions with effects
- Generic functions

Closures will use a separate syntax decision (see WEP 2026-01-16: Closure Implementation), but the arrow syntax remains consistent for any return type annotations.

## Consequences

### Positive

- **Better readability** for complex type signatures involving effects, higher-order functions, and generics
- **Consistency** with Rust and the WebAssembly/systems programming ecosystem
- **Mathematical clarity** with directional semantics (`input -> output`)
- **Future-proof** for advanced type system features (dependent types, effect handlers)
- **Visual distinction** between parameter types (`:`) and return types (`->`)

### Negative

- **Learning curve** for developers coming from TypeScript/JavaScript background
- **Different from web development** conventions, potentially slowing adoption from the web ecosystem
- **Mixed syntax** with closures if they use different notation (mitigated by separate design decision)

### Neutral

- Generic closure support is deprioritized, so the lack of generic closure syntax in Rust-style is not a concern
- The choice is primarily aesthetic and does not impact runtime performance or compilation complexity

## Alternatives Considered

1. **TypeScript-style colon (`:`)** - Rejected due to reduced clarity in higher-order functions and weaker alignment with systems programming languages
2. **Hybrid approach** (`:` for functions, `->` for closures) - Rejected due to inconsistency
3. **No syntax (positional like Go)** - Rejected as it reduces type annotation explicitness

## Related Decisions

- WEP 2026-01-16: Closure Implementation - Defines closure syntax separately
- WEP 2026-01-13: Struct and Trait System - Uses consistent type annotation patterns
- WEP 2026-01-11: Operator Precedence and Associativity - Type expressions in function signatures

## References

- [Rust Internals: What is the actual reason of having `->` before return type](https://internals.rust-lang.org/t/what-is-the-actual-reason-of-having-before-return-type/12481)
- [Kotlin Discussions: We should use -> instead of :](https://discuss.kotlinlang.org/t/we-should-use-instead-of-to-denote-function-return-type/24218)
- [Wikipedia: Trailing return type](https://en.wikipedia.org/wiki/Trailing_return_type)
- [PEP 484: Type Hints (Python)](https://peps.python.org/pep-0484/)
