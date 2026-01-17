# WEP: Target WASI P3 Only

## Status

Accepted

## Context

Wado is a new programming language targeting Wasm/WASI. WASI has three major versions:

- **WASI P1 (Preview 1)**: Legacy API with POSIX-like syscalls
- **WASI P2 (Preview 2)**: Component Model-based, uses `pollable` for async
- **WASI P3 (Preview 3)**: Native `stream<T>`, `future<T>`, and `async func`

The question is whether Wado should support multiple WASI versions or focus on a single target.

### WASI P2 vs P3 Differences

The async model differs fundamentally:

```wit
// P2: Returns a pollable stream handle, caller manages polling
interface stdout {
    get-stdout: func() -> output-stream;
}

// P3: Takes a stream, runtime handles async transparently
interface stdout {
    write-via-stream: async func(data: stream<u8>) -> result<_, error-code>;
}
```

P2 requires explicit `subscribe()` calls and pollable management. P3's `async func` maps directly to Wasm Stack Switching, enabling colorless async.

## Decision

**Wado targets WASI P3 only.** It will not support WASI P1 or P2.

### Rationale

1. **Design alignment**: Wado's core abstractions map directly to P3 primitives:
   - `Stream<T>` ↔ P3's `stream<T>`
   - `Future<T>` ↔ P3's `future<T>`
   - Colorless async ↔ P3's `async func` + stack switching

2. **Zero abstraction philosophy**: Supporting multiple WASI versions would require abstraction layers, contradicting Wado's "zero abstraction to Wasm" design principle.

3. **No legacy burden**: Wado is a new language with no existing users on P1/P2. There's no migration cost.

4. **Implementation simplicity**: One target means cleaner compiler implementation, standard library, and documentation.

5. **Runtime support**: wasmtime v40+ supports P3 with `-W component-model-async=y`. Other runtimes are following.

6. **Future-oriented**: P3 is where WASI is heading. Designing for the past would require workarounds that compromise the language design.

## Consequences

### Positive

- Clean mapping between Wado types and WASI types
- Simpler compiler and standard library implementation
- Colorless async works naturally without compatibility shims
- Documentation and examples are straightforward

### Negative

- Cannot run on runtimes that only support P1/P2
- Early adopters must use wasmtime v40+ or other P3-capable runtimes
- Fewer deployment targets initially

### Neutral

- Standard library (`wasi/`) only needs P3 WIT definitions
- The `vendor/wasmtime/crates/wasi/src/p3/wit/` directory is the sole source of truth for WASI interfaces

## References

- WASI P3 proposal: https://github.com/WebAssembly/WASI/blob/main/wasip3/README.md
- wasmtime P3 support: `vendor/wasmtime/crates/wasi/src/p3/wit/`
- Wado spec WASI section: `spec.md` (WASI / Browser Support)
