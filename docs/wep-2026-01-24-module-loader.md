# WEP: Module Loader Design

## Context

Wado needs a well-defined module loading system that:

- Clearly distinguishes between different module namespaces
- Provides immediate and helpful error messages for invalid module paths
- Delegates actual I/O operations to the CompilerHost abstraction
- Supports future extensibility (remote modules, wasm imports, etc.)

Currently, unknown namespaces (e.g., `unknown:foo`) are silently treated as local file paths, resulting in confusing "file not found" errors instead of immediate "unknown namespace" errors.

## Decision

> The namespace grammar and the "unknown namespace = error" rule below are
> superseded by [Package and Module Specifier Syntax](./wep-2026-06-17-package-module-syntax.md):
> a namespace is reserved iff the compiler bundles it (`wasi`, `core`), every
> other coordinate namespace is open, and `lib:` is the single indirection
> namespace. The local / remote / I/O-delegation rules below still hold.

### Module Path Syntax

Module paths in `use` declarations follow this grammar:

```
ModulePath := CoordinatePath | LibAlias | LocalPath | RemotePath

CoordinatePath := Namespace (":" Namespace)* ":" Package ("@" Version)?
LibAlias := "lib:" Identifier
Namespace, Package := Identifier

LocalPath := ("." | "..") "/" RelativePath
RelativePath := PathSegment ("/" PathSegment)*
PathSegment := <any valid path component>

RemotePath := ("http://" | "https://") Url
```

### Namespace Resolution Rules

1. **Reserved namespace ⇔ bundled namespace**
   - `core:` — Wado standard library (`core:prelude`, `core:cli`, etc.)
   - `wasi:` — WASI interface modules (`wasi:cli`, `wasi:filesystem`, etc.)
   - Every other coordinate namespace is **open** and resolves from outside
     (default registry, or `with`/manifest source override).
   - `lib:` is the single indirection namespace (alias / rename / private dep).
   - See [Package and Module Specifier Syntax](./wep-2026-06-17-package-module-syntax.md)
     for resolution and version rules.

2. **Remote Modules (`http://` or `https://`)**
   - URLs starting with `http://` or `https://` are remote modules
   - Resolution is delegated to CompilerHost
   - CompilerHost may:
     - Fetch from network
     - Use cached version
     - Reject with appropriate error
   - Security considerations are handled by CompilerHost implementation

3. **Local Modules (`./` or `../`)**
   - Paths starting with `./` or `../` are local modules
   - Resolution is relative to the importing module
   - Actual file loading is delegated to CompilerHost
   - Path normalization follows RFC 3986 for `.` and `..` resolution

4. **Invalid Paths**
   - Paths that don't match any of the above patterns are invalid
   - Examples of invalid paths:
     - `foo` (no prefix)
     - `/absolute/path` (absolute paths not allowed)
     - `file.wado` (no `./` prefix)
   - Error message: `invalid module path 'xxx'; use './' for local modules or 'namespace:' for library modules`

### ModuleSource Representation

The internal representation of module sources:

```rust
pub enum ModuleSource {
    /// Core library module (core:prelude, core:cli, etc.)
    Core { name: String },

    /// WASI interface module (wasi:cli, wasi:filesystem, etc.)
    Wasi { interface: String },

    /// Local module relative to project (./module.wado, ../lib.wado)
    Local { path: String },

    /// Remote module (https://example.com/lib.wado)
    Remote { url: String },

    /// Entry point module
    EntryPoint { filename: Option<String> },
}
```

### Error Handling

New error type for module resolution:

```rust
pub enum LoadError {
    // ... existing variants ...

    /// Unknown module namespace
    UnknownNamespace { namespace: String },

    /// Invalid module path format
    InvalidModulePath { path: String },
}
```

### CompilerHost Responsibilities

The CompilerHost trait handles actual I/O:

```rust
pub trait CompilerHost {
    /// Load source code from a local path (./xxx, ../xxx)
    async fn load_source(&self, path: &str) -> Result<String, SourceError>;

    /// Load source code from a remote URL (http://, https://)
    async fn load_remote(&self, url: &str) -> Result<String, SourceError>;
}
```

- Standard library modules (`core:*`, `wasi:*`) are NOT passed to CompilerHost
- They are resolved from embedded sources within the compiler

### Resolution Flow

```
use {foo} from "xxx:yyy"
    │
    ├─ "core:*"   → ModuleSource::Core    → embedded stdlib
    ├─ "wasi:*"   → ModuleSource::Wasi    → embedded stdlib
    ├─ "http://*" → ModuleSource::Remote  → host.load_remote()
    ├─ "https://*"→ ModuleSource::Remote  → host.load_remote()
    ├─ "./*"      → ModuleSource::Local   → host.load_source()
    ├─ "../*"     → ModuleSource::Local   → host.load_source()
    ├─ "xxx:*"    → ERROR: unknown namespace
    └─ other      → ERROR: invalid module path
```

## Consequences

### Positive

- Clear and immediate error messages for invalid module paths
- Extensible namespace system (easy to add new namespaces in the future)
- Clean separation between module resolution and I/O
- Security: remote modules explicitly marked and handled by CompilerHost

### Negative

- Breaking change: `use {foo} from "module.wado"` now requires `./` prefix
- More verbose local imports

### Migration

Code using bare filenames needs updating:

```wado
// Before (now invalid)
use {foo} from "utils.wado";

// After
use {foo} from "./utils.wado";
```

## Future Considerations

### Additional Namespaces

Future versions may add:

- `npm:` - npm package imports
- `jsr:` - JSR package imports
- `pkg:` - Generic package manager imports

### Import Attributes

For non-Wado file imports:

```wado
use helper from "./helper.wasm" with { type: "wasm" };
```

See WEP-2026-01-10-wasm-import for details. JSON file import is no longer a core import type — it is lowered by a Kiln generator (WEP: Kiln).

## Implementation Status

- [x] Add `Remote` variant to `ModuleSource`
- [x] Add `UnknownNamespace` and `InvalidModulePath` to `LoadError`
- [x] Update `resolve_import` to validate namespaces
- [x] Update `CompilerHost::load_source` to document URL support
- [x] Update error messages
- [x] Add tests for namespace validation
