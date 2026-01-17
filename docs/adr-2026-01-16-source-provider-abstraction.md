# ADR: SourceProvider Abstraction for Compiler I/O

**Status**: Proposed
**Date**: 2026-01-16

## Context

The Wado compiler currently has direct filesystem dependencies for loading user modules:

```rust
// Current implementation
fn load_module(path: &str) -> Result<String> {
    std::fs::read_to_string(path)?
}
```

This creates several limitations:

1. **Browser execution impossible**: Cannot compile to `wasm32-unknown-unknown` target
2. **LSP integration complexity**: Must persist unsaved buffers to temporary files
3. **Testing overhead**: Requires creating temporary files for test fixtures
4. **Embedding difficulty**: Hard to use compiler in sandboxed environments
5. **Security concerns**: Compiler core has unrestricted filesystem access

However, the compiler already handles standard library (`core:*`, `wasi:*`) modules via compile-time embedding using `include_str!`, which works well and should be preserved.

## Decision

Introduce an async `SourceProvider` trait to abstract source code retrieval:

```rust
pub trait SourceProvider {
    /// Provide source code for a user module
    ///
    /// # Arguments
    /// * `path` - Normalized module path (e.g., "./lib.wado", "../utils.wado")
    ///   NOTE: Standard library paths (core:*, wasi:*) are NOT passed to this method.
    ///   They are handled directly by the compiler via embedded sources.
    ///
    /// # Returns
    /// The complete source code including __DATA__ section if present
    async fn provide(&self, path: &str) -> Result<String, SourceError>;
}
```

### Key Design Decisions

1. **Async by default**: Enables HTTP fetching, remote file access, and future parallelization
2. **No `from` parameter**: Path normalization happens before `provide()` call
3. **Standard library handled by compiler**: `core:*` and `wasi:*` resolved before calling SourceProvider
4. **Simple responsibility**: SourceProvider only retrieves user code, no special handling

### Responsibility Distribution

| Responsibility                        | Owner                                  |
| ------------------------------------- | -------------------------------------- |
| Standard library (`core:*`, `wasi:*`) | Compiler (embedded via `include_str!`) |
| User code (`./`, `../`)               | SourceProvider                         |
| Path normalization                    | Compiler (before `provide()` call)     |
| Circular dependency detection         | Compiler                               |
| Parsed module caching                 | Compiler                               |
| Source string caching                 | SourceProvider (optional)              |

### Implementation Structure

```
wado-compiler/              # I/O free (wasm32 compatible)
  src/
    source_provider.rs      # Trait definition + error types
    loader.rs               # Uses SourceProvider
    stdlib.rs               # Embedded standard library (unchanged)

wado-cli/                   # Filesystem dependent
  src/
    fs_source_provider.rs   # FileSystemSourceProvider implementation
    compile.rs              # Creates provider, calls compiler

wado-browser/               # Future: browser environment
  src/
    memory_source_provider.rs  # InMemorySourceProvider
```

### Example Implementations

**FileSystemSourceProvider** (wado-cli):

```rust
pub struct FileSystemSourceProvider {
    base_path: PathBuf,
}

impl SourceProvider for FileSystemSourceProvider {
    async fn provide(&self, path: &str) -> Result<String, SourceError> {
        let full_path = self.base_path.join(path);
        std::fs::read_to_string(&full_path)
            .map_err(|e| SourceError::IoError {
                path: path.to_string(),
                message: e.to_string(),
            })
    }
}
```

**InMemorySourceProvider** (future use):

```rust
pub struct InMemorySourceProvider {
    sources: HashMap<String, String>,
}

impl SourceProvider for InMemorySourceProvider {
    async fn provide(&self, path: &str) -> Result<String, SourceError> {
        self.sources
            .get(path)
            .cloned()
            .ok_or_else(|| SourceError::NotFound {
                path: path.to_string(),
            })
    }
}
```

### Compiler Integration

```rust
// Compiler side
impl ModuleLoader {
    async fn load_module(&mut self, path: &str) -> Result<Module> {
        // 1. Check standard library first (embedded)
        if let Some(source) = stdlib::get_stdlib_module(path) {
            return self.parse_module(path, source);
        }

        // 2. Get from SourceProvider (user code)
        let source = self.source_provider.provide(path).await?;
        self.parse_module(path, &source)
    }
}
```

### CLI Usage (unchanged from user perspective)

```bash
# User experience remains identical
wado compile main.wado -o output.wasm
```

Internal implementation:

```rust
// CLI side
pub fn compile_command(path: &Path, options: CompileOptions) -> Result<()> {
    let source = std::fs::read_to_string(path)?;
    let provider = FileSystemSourceProvider::new(path.parent().unwrap());

    // Compiler core is I/O free
    let wasm = futures::executor::block_on(
        wado_compiler::compile_sources(&source, path, &provider, options)
    )?;

    std::fs::write(&output_path, wasm)?;
    Ok(())
}
```

## Consequences

### Benefits

1. **Browser compilation enabled**
   - Compile to `wasm32-unknown-unknown`
   - Build online playground with real-time compilation
   - No server required for compilation

2. **LSP integration simplified**
   - Pass unsaved editor buffers directly
   - No temporary file creation
   - Real-time diagnostics on unsaved changes

3. **Testing simplified**
   - E2E tests remain filesystem-based (preferable for git management)
   - Unit tests can use `InMemorySourceProvider` for dynamically generated code

4. **Security boundary clarified**
   - Compiler core has zero filesystem access
   - I/O responsibility isolated to CLI/provider layer

5. **Embedding flexibility**
   - Use in game engines, build tools, etc.
   - Custom providers for database storage, network fetching, etc.

6. **Incremental compilation compatible**
   - Similar to Rust's query system, TypeScript's module resolution, LLVM's VFS
   - Caching can be implemented at multiple layers
   - Content-hash based change detection (more reliable than timestamps)

### Trade-offs

1. **Internal implementation complexity**
   - Trait design and error handling abstraction required
   - One-time migration cost for existing loader code

2. **Async runtime requirement**
   - CLI uses `futures::executor::block_on` (lightweight)
   - No heavy runtime (tokio) needed for CLI

3. **Conditional compilation for targets**
   ```rust
   #[cfg(not(target_arch = "wasm32"))]
   pub struct FileSystemSourceProvider { ... }
   ```

### Non-Issues

1. **Developer experience (CLI)**: Unchanged - `FileSystemSourceProvider` is transparent
2. **Test fixtures**: Remain filesystem-based - easier to manage and review
3. **Incremental compilation**: Not hindered - follows industry best practices (Rust, TypeScript, LLVM)
4. **Standard library**: Embedding preserved - compiler maintains control

## Alternatives Considered

### Alternative 1: Keep filesystem dependencies

**Rejected**: Blocks browser and LSP use cases, which are valuable features.

### Alternative 2: Standard library in SourceProvider

```rust
impl SourceProvider for FileSystemSourceProvider {
    async fn provide(&self, path: &str) -> Result<String> {
        // Handle both stdlib and user code
        if let Some(stdlib) = get_stdlib(path) {
            return Ok(stdlib);
        }
        // ...
    }
}
```

**Rejected**:

- Mixes responsibilities
- Every provider must know about `core:*` and `wasi:*`
- Loses compile-time embedding benefits
- `InMemorySourceProvider` would need stdlib duplicated

### Alternative 3: Synchronous trait

```rust
pub trait SourceProvider {
    fn provide(&self, path: &str) -> Result<String>;
}
```

**Rejected**: Cannot support HTTP fetching, async file I/O, or remote providers.

## Implementation Plan

1. **Phase 1**: Define `SourceProvider` trait in `wado-compiler/src/source_provider.rs`
2. **Phase 2**: Refactor `loader.rs` to accept `&dyn SourceProvider`
3. **Phase 3**: Implement `FileSystemSourceProvider` in `wado-cli`
4. **Phase 4**: Update CLI commands to use new API
5. **Phase 5**: Verify E2E tests still pass (no changes needed to test files)
6. **Phase 6**: Add `wasm32-unknown-unknown` build target
7. **Future**: Implement `InMemorySourceProvider` for browser/LSP

## References

- [Rust incremental compilation](https://rustc-dev-guide.rust-lang.org/queries/incremental-compilation.html) - Query-based system with VFS-like abstractions
- [TypeScript incremental compilation](https://www.typescriptlang.org/tsconfig/incremental.html) - Module resolution abstraction
- [LLVM VFS proposal](https://discourse.llvm.org/t/rfc-write-support-for-llvm-virtual-file-system-vfs-to-virtualize-compiler-outputs/65110) - Virtual filesystem for compiler I/O
