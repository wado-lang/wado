# Re-export Syntax (`pub use`)

## Context

Wado needs a way to re-export symbols from other modules. This is a common pattern in module systems where a library wants to expose items from its dependencies or internal modules as part of its public API.

### Terminology

Wado distinguishes between two kinds of "public":

| Keyword | Term | Meaning |
|---------|------|---------|
| `pub` | **module public** | Visible to other Wado modules that import this module |
| `export` | **world export** | Exposed at the Component Model boundary (WASI world conformance) |

The `pub use` syntax combines module public visibility with import, creating a **re-export**.

### Motivation

Without re-export, users must import from the original source:

```wado
// math/internal/trig.wado
pub fn sin(x: f64) -> f64 { ... }
pub fn cos(x: f64) -> f64 { ... }

// math/internal/exp.wado
pub fn exp(x: f64) -> f64 { ... }
pub fn log(x: f64) -> f64 { ... }

// User must know internal structure
use {sin, cos} from "math/internal/trig.wado";
use {exp, log} from "math/internal/exp.wado";
```

With re-export, the library can provide a unified API:

```wado
// math/mod.wado
pub use {sin, cos} from "./internal/trig.wado";
pub use {exp, log} from "./internal/exp.wado";

// User imports from the public API
use {sin, cos, exp, log} from "math";
```

## Decision

### Syntax

Re-export uses `pub use` followed by the standard import syntax:

```wado
// Re-export specific items
pub use {foo, bar} from "./internal.wado";

// Re-export with rename
pub use {internal_name as public_name} from "./internal.wado";

// Re-export from core/wasi modules
pub use {Option, Result} from "core:prelude";
pub use {Stdout} from "wasi:cli";
```

### Semantics

1. `pub use` makes imported symbols available to modules that import this module
2. The re-exported symbol behaves identically to the original
3. Re-exports are resolved at compile time (no runtime cost)
4. Circular re-exports are prohibited

### Visibility Levels

| Declaration | Within module | Other Wado modules | CM world boundary |
|-------------|---------------|-------------------|-------------------|
| `fn foo()` | Yes | No | No |
| `pub fn foo()` | Yes | Yes | No |
| `export fn foo()` | Yes | No | Yes |
| `pub export fn foo()` | Yes | Yes | Yes |
| `use {x} from "..."` | Yes | No | - |
| `pub use {x} from "..."` | Yes | Yes | - |

### Namespace Re-export

Namespace re-export is also supported:

```wado
// Re-export entire module as namespace
pub use utils from "./utils.wado";

// Users can then access via namespace
use {utils} from "mylib";
utils::helper();
```

### Restrictions

1. Cannot re-export private (non-`pub`) items from other modules
2. Cannot add `export` to re-exports (re-exports are module-level only)
3. Wildcard re-export (`pub use * from "..."`) is not supported (consistent with import rules)

## Consequences

### Positive

- Libraries can organize code internally while presenting a clean public API
- Refactoring internal module structure doesn't break users
- Enables facade pattern for complex libraries
- Consistent with Rust's `pub use` semantics

### Negative

- Adds complexity to the module system
- Symbol resolution must track re-export chains
- Error messages must show both original and re-export locations

### Implementation Notes

1. Parser: Extend `use` statement to accept optional `pub` prefix
2. Symbol table: Track re-export relationships
3. Name resolution: Follow re-export chains during lookup
4. Error reporting: Include re-export path in "symbol not found" errors

## Not in Scope

- `pub(crate)` or other visibility modifiers (Wado has no crate concept)
- Glob re-exports (`pub use * from "..."`)
- Re-exporting with `export` for CM boundary exposure
