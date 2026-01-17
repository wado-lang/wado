# WEP: CompilerHost Abstraction for Compiler I/O

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
6. **Diagnostics coupling**: Error messages are directly printed to stderr, making it difficult to integrate with IDEs or collect errors programmatically

However, the compiler already handles standard library (`core:*`, `wasi:*`) modules via compile-time embedding using `include_str!`, which works well and should be preserved.

## Decision

Introduce an async `CompilerHost` trait to abstract both source code retrieval and diagnostic output:

```rust
pub trait CompilerHost {
    /// Load source code for a user module
    ///
    /// # Arguments
    /// * `path` - Normalized module path (e.g., "./lib.wado", "../utils.wado")
    ///   NOTE: Standard library paths (core:*, wasi:*) are NOT passed to this method.
    ///   They are handled directly by the compiler via embedded sources.
    ///
    /// # Returns
    /// The complete source code including __DATA__ section if present
    async fn load_source(&self, path: &str) -> Result<String, SourceError>;

    /// Emit a diagnostic (error, warning, etc.)
    ///
    /// This method is called by the compiler whenever a diagnostic needs to be reported.
    /// Implementations can print to stderr, collect into a list, send to an LSP client, etc.
    async fn emit_diagnostic(&mut self, diagnostic: Diagnostic);
}
```

The `Diagnostic` structure in Wado:

```wado
pub struct Diagnostic {
    pub severity: Severity,  // Error, Warning, Info, Hint
    pub code: ErrorCode,
    pub message: String,
    pub span: Option<Span>,  // source location
}
```

### Key Design Decisions

1. **Async by default**: Enables HTTP fetching, remote file access, browser UI updates, and future parallelization
2. **Unified I/O abstraction**: Source loading and diagnostic output are always needed together in I/O-less environments
3. **Standard library handled by compiler**: `core:*` and `wasi:*` resolved before calling CompilerHost
4. **Named after TypeScript convention**: `CompilerHost` is a well-known term in compiler tooling

### Responsibility Distribution

| Responsibility                        | Owner                                  |
| ------------------------------------- | -------------------------------------- |
| Standard library (`core:*`, `wasi:*`) | Compiler (embedded via `include_str!`) |
| User code (`./`, `../`)               | CompilerHost (`load_source`)           |
| Path normalization                    | Compiler (before `load_source()` call) |
| Circular dependency detection         | Compiler                               |
| Parsed module caching                 | Compiler                               |
| Source string caching                 | CompilerHost (optional)                |
| Diagnostic formatting                 | CompilerHost (`emit_diagnostic`)       |
| Diagnostic collection/display         | CompilerHost                           |

### Implementation Structure

```
wado-compiler/              # I/O free (wasm32 compatible)
  src/
    compiler_host.rs        # Trait definition + Diagnostic types
    loader.rs               # Uses CompilerHost
    stdlib.rs               # Embedded standard library (unchanged)

wado-cli/                   # Filesystem dependent
  src/
    cli_compiler_host.rs    # CliCompilerHost implementation
    compile.rs              # Creates host, calls compiler

wado-browser/               # Future: browser environment
  src/
    browser_compiler_host.rs  # BrowserCompilerHost
```

### Example Implementations

**CliCompilerHost** (wado-cli):

```rust
pub struct CliCompilerHost {
    base_path: PathBuf,
}

impl CompilerHost for CliCompilerHost {
    async fn load_source(&self, path: &str) -> Result<String, SourceError> {
        let full_path = self.base_path.join(path);
        std::fs::read_to_string(&full_path)
            .map_err(|e| SourceError::IoError {
                path: path.to_string(),
                message: e.to_string(),
            })
    }

    async fn emit_diagnostic(&mut self, diagnostic: Diagnostic) {
        // Format and print to stderr
        eprintln!("{}: {}", diagnostic.severity, diagnostic.message);
        if let Some(span) = &diagnostic.span {
            eprintln!("  --> {}:{}:{}", span.file, span.line, span.column);
        }
    }
}
```

**BrowserCompilerHost** (future use):

```rust
pub struct BrowserCompilerHost {
    sources: HashMap<String, String>,
    diagnostics: Vec<Diagnostic>,
}

impl CompilerHost for BrowserCompilerHost {
    async fn load_source(&self, path: &str) -> Result<String, SourceError> {
        self.sources
            .get(path)
            .cloned()
            .ok_or_else(|| SourceError::NotFound {
                path: path.to_string(),
            })
    }

    async fn emit_diagnostic(&mut self, diagnostic: Diagnostic) {
        // Collect for later display in UI
        self.diagnostics.push(diagnostic);
    }
}
```

### Compiler Integration

```rust
// Compiler side
impl Compiler {
    async fn load_module(&mut self, path: &str) -> Result<Module> {
        // 1. Check standard library first (embedded)
        if let Some(source) = stdlib::get_stdlib_module(path) {
            return self.parse_module(path, source);
        }

        // 2. Get from CompilerHost (user code)
        let source = self.host.load_source(path).await?;
        self.parse_module(path, &source)
    }

    async fn report_error(&mut self, span: Option<Span>, code: ErrorCode, message: String) {
        self.host.emit_diagnostic(Diagnostic {
            severity: Severity::Error,
            code,
            message,
            span,
        }).await;
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
    let mut host = CliCompilerHost::new(path.parent().unwrap());

    // Compiler core is I/O free
    let wasm = futures::executor::block_on(
        wado_compiler::compile_sources(&source, path, &mut host, options)
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
   - Diagnostics displayed directly in browser UI

2. **LSP integration simplified**
   - Pass unsaved editor buffers directly
   - No temporary file creation
   - Real-time diagnostics on unsaved changes
   - Structured diagnostics with source locations for IDE integration

3. **Testing simplified**
   - E2E tests remain filesystem-based (preferable for git management)
   - Unit tests can use in-memory host for dynamically generated code
   - Diagnostics can be collected and asserted programmatically

4. **Security boundary clarified**
   - Compiler core has zero filesystem access
   - I/O responsibility isolated to CLI/host layer

5. **Embedding flexibility**
   - Use in game engines, build tools, etc.
   - Custom hosts for database storage, network fetching, etc.
   - Custom diagnostic formatting and routing

6. **Incremental compilation compatible**
   - Similar to Rust's query system, TypeScript's module resolution, LLVM's VFS
   - Caching can be implemented at multiple layers
   - Content-hash based change detection (more reliable than timestamps)

### Trade-offs

1. **Internal implementation complexity**
   - Trait design and error handling abstraction required
   - One-time migration cost for existing loader and error reporting code

2. **Async runtime requirement**
   - CLI uses `futures::executor::block_on` (lightweight)
   - No heavy runtime (tokio) needed for CLI

3. **Conditional compilation for targets**
   ```rust
   #[cfg(not(target_arch = "wasm32"))]
   pub struct CliCompilerHost { ... }
   ```

### Non-Issues

1. **Developer experience (CLI)**: Unchanged - `CliCompilerHost` is transparent
2. **Test fixtures**: Remain filesystem-based - easier to manage and review
3. **Incremental compilation**: Not hindered - follows industry best practices (Rust, TypeScript, LLVM)
4. **Standard library**: Embedding preserved - compiler maintains control
5. **Diagnostic quality**: The `Diagnostic` struct provides rich information for any output format

## Alternatives Considered

### Alternative 1: Keep filesystem dependencies

**Rejected**: Blocks browser and LSP use cases, which are valuable features.

### Alternative 2: Standard library in CompilerHost

```rust
impl CompilerHost for CliCompilerHost {
    async fn load_source(&self, path: &str) -> Result<String> {
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
- Every host must know about `core:*` and `wasi:*`
- Loses compile-time embedding benefits
- `BrowserCompilerHost` would need stdlib duplicated

### Alternative 3: Synchronous trait

```rust
pub trait CompilerHost {
    fn load_source(&self, path: &str) -> Result<String>;
    fn emit_diagnostic(&mut self, diagnostic: Diagnostic);
}
```

**Rejected**: Cannot support HTTP fetching, async file I/O, browser UI updates, or remote providers.

### Alternative 4: Separate traits for source loading and diagnostics

```rust
pub trait SourceLoader {
    async fn load(&self, path: &str) -> Result<String, SourceError>;
}

pub trait DiagnosticConsumer {
    async fn consume(&mut self, diagnostic: Diagnostic);
}
```

**Rejected**:

- In I/O-less environments (browser, LSP), both are always needed together
- Adds API complexity without practical benefit
- No real use case for mixing different source loaders with different diagnostic consumers

## Implementation Plan

1. **Phase 1**: Define `CompilerHost` trait and `Diagnostic` types in `wado-compiler/src/compiler_host.rs`
2. **Phase 2**: Refactor compiler to accept `&mut dyn CompilerHost`
3. **Phase 3**: Migrate error reporting to use `emit_diagnostic`
4. **Phase 4**: Implement `CliCompilerHost` in `wado-cli`
5. **Phase 5**: Update CLI commands to use new API
6. **Phase 6**: Verify E2E tests still pass (no changes needed to test files)
7. **Phase 7**: Add `wasm32-unknown-unknown` build target
8. **Future**: Implement `BrowserCompilerHost` for browser/LSP

## References

- [TypeScript CompilerHost](https://github.com/microsoft/TypeScript/wiki/Using-the-Compiler-API) - Unified I/O abstraction for source loading and more
- [Clang DiagnosticConsumer](https://clang.llvm.org/doxygen/classclang_1_1DiagnosticConsumer.html) - Abstract interface for diagnostic output
- [Swift DiagnosticEngine](https://www.swift.org/blog/new-diagnostic-arch-overview/) - Announcer/listener pattern for diagnostics
- [Rust Emitter trait](https://github.com/rust-lang/rust/blob/master/compiler/rustc_errors/src/emitter.rs) - Diagnostic emission interface
- [Rust incremental compilation](https://rustc-dev-guide.rust-lang.org/queries/incremental-compilation.html) - Query-based system with VFS-like abstractions
- [LLVM VFS proposal](https://discourse.llvm.org/t/rfc-write-support-for-llvm-virtual-file-system-vfs-to-virtualize-compiler-outputs/65110) - Virtual filesystem for compiler I/O
