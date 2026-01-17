# WEP: Compiler Pipeline Refactoring
## Status
Proposed

## Context

The current Wado compiler has a straightforward pipeline:

```
lexer → parser → analyzer → desugar → codegen → wasm
```

While functional, this architecture has several limitations:

1. **Mixed responsibilities**: The `analyzer` phase combines local name resolution with module resolution and type checking, making it difficult to reason about and test independently.

2. **No typed intermediate representation**: Code generation works directly from the AST, requiring inline type inference and resolution during codegen. This couples transformation logic with code emission.

3. **Limited optimization opportunities**: There is no dedicated optimization phase. Any optimizations must be embedded in codegen, mixing concerns.

4. **Poor cacheability**: The current phase boundaries don't align well with caching opportunities. Module resolution happens early, meaning cached artifacts would need to encode cross-module references.

5. **Sequential processing**: The architecture doesn't naturally support parallel compilation of independent modules.

## Decision

Refactor the compiler into the following eight-phase pipeline:

```
lexer → parser → bind → desugar → resolve → lower → optimize → codegen
  ↓        ↓       ↓        ↓         ↓        ↓         ↓         ↓
tokens   AST     AST      AST       TIR      TIR       TIR      wasm
              (scoped) (simple)  (typed)  (lowered)   (opt)
```

### Phase Definitions

#### 1. Lexer (unchanged)

Tokenizes source code into a token stream. Preserves comments and extracts `__DATA__` section.

- Input: Source code string
- Output: Token stream, comments, data section

#### 2. Parser (unchanged)

Recursive descent parser producing an abstract syntax tree.

- Input: Token stream
- Output: AST (`Module`)

#### 3. Bind (new)

Local name resolution and scope analysis. This phase operates on a single module without loading dependencies.

Responsibilities:

- Build scope chains for local variables
- Detect use-before-define errors
- Detect duplicate definitions within a scope
- Resolve local name references to scope-local bindings
- Basic sanity checks (syntactic validity beyond parsing)

- Input: AST
- Output: AST with scope annotations

#### 4. Desugar (expanded)

Syntactic transformations that do not require type information. Operates on a single module in isolation.

Responsibilities:

- Compound assignment expansion (`x += 1` → `x = x + 1`)
- Comparison chain expansion (`a < b < c` → `a < b && b < c`)
- JSX syntax transformation (structure only, types resolved later)
- Reactive signal syntax transformation (markers only, graph built later)
- Template string expansion
- Other syntactic sugar removal

Key property: The output is self-contained per module with no cross-module references, making it suitable as a cache boundary.

- Input: Scoped AST
- Output: Desugared AST

#### 5. Resolve (refactored)

Module resolution and type checking. This is the first phase that requires cross-module information.

Responsibilities:

- Load and resolve imported modules
- Build the module dependency graph
- Type inference and type checking
- Resolve all type references to concrete types
- Resolve all name references to symbol IDs
- Produce the typed intermediate representation

- Input: Desugared AST (for all modules in dependency order)
- Output: Typed IR (TIR)

#### 6. Lower (new)

Type-driven transformations. Uses full type information to perform semantic lowering.

Responsibilities:

- Reactive signal dependency graph construction
- Method call resolution (direct vs. effect operation)
- Generic instantiation / monomorphization
- Closure capture analysis
- JSX element type binding

- Input: TIR
- Output: Lowered TIR

#### 7. Optimize (new)

IR-to-IR optimizations.

Responsibilities:

- Dead code elimination
- Constant folding and propagation
- Inlining (within optimization budget)
- Link-time optimization across modules

- Input: Lowered TIR
- Output: Optimized TIR

#### 8. Codegen (simplified)

Pure translation from IR to WebAssembly. No transformations or type resolution—just mechanical conversion.

Responsibilities:

- Emit Wasm instructions from TIR
- Generate Component Model structure
- Link modules into final component

- Input: Optimized TIR
- Output: Component Model WebAssembly bytes

### Typed Intermediate Representation (TIR)

A new IR with structures parallel to the AST, but with all names resolved to IDs and all expressions annotated with their resolved types.

Key differences from AST:

- No string-based name lookups; all references are IDs/indices
- Every expression carries its resolved type
- Call targets distinguish between direct calls, effect operations, and closures
- No syntactic sugar remains

### Cache Boundary

The desugared AST (output of phase 4) serves as the cache boundary:

- Self-contained per module (no cross-module references)
- Invalidated only when source changes (not when dependencies change)
- Enables skipping phases 1-4 for unchanged modules

### Parallelism Model

The pipeline supports two forms of parallelism:

1. **Pre-resolve parallelism**: Phases 1-4 (lexer through desugar) can process all modules in parallel since they operate on single modules without cross-module dependencies.

2. **Post-resolve parallelism**: Phases 6-8 (lower through codegen) can process functions in parallel once all types are resolved.

3. **Resolve phase**: Processes modules in dependency order, but modules at the same dependency level can be processed in parallel.

## Consequences

### Benefits

1. **Separation of concerns**: Each phase has a clear, single responsibility. Local binding is separate from module resolution. Syntactic transformations are separate from semantic transformations.

2. **Testability**: Phases can be tested in isolation. The bind phase can be tested without loading modules. Desugar can be tested without type information.

3. **Typed IR enables optimization**: With a dedicated IR where all types are resolved, optimization passes can make informed decisions without re-inferring types.

4. **Simpler codegen**: Code generation becomes mechanical translation, reducing complexity and potential for bugs.

5. **Cache-friendly architecture**: The desugared AST is an ideal cache point, enabling fast incremental builds.

6. **Parallelism-ready**: The phase boundaries align with natural parallelization opportunities.

### Costs

1. **Implementation effort**: Significant refactoring required. New TIR types parallel to AST. New bind and lower phases.

2. **Memory usage**: Maintaining both AST and TIR increases peak memory. Mitigated by dropping AST after resolve.

3. **Additional passes**: More phases means more passes over the code. Mitigated by each pass being simpler and more focused.

### Migration Strategy

The refactoring can be done incrementally:

1. Extract `bind` from current `analyzer`
2. Expand `desugar` with additional transformations
3. Introduce TIR and refactor `resolve` to produce it
4. Introduce `lower` phase
5. Introduce `optimize` phase (can start empty)
6. Simplify `codegen` to work from TIR

Each step can be validated independently before proceeding to the next.

### Future Opportunities

This architecture enables future enhancements:

- Incremental compilation via desugared AST caching
- Parallel compilation of independent modules
- Language server protocol support (phases 1-4 provide fast feedback)
- Additional optimization passes
- Alternative backends (interpret TIR directly for debugging)
