# WEP: Visibility — `internal` / `pub` / `export`

## Context

Wado had two visibility keywords: `pub` (visible to other Wado modules in the
same package) and `export` (visible at the Component Model boundary). `export`
carried two unrelated jobs at once:

- **Library boundary** — the package's public API, consumed by other _Wado_
  packages.
- **CM boundary** — the ABI surface lowered through the Canonical ABI and
  emitted into WIT.

These are not the same thing. The CM ABI cannot represent closures (`fn`
values), generics (CM is monomorphic), traits / dynamic dispatch, or effect
polymorphism (`<effect E>`). Because crossing a package boundary required
`export`, and those constructs cannot be `export`ed, a generic, higher-order,
or trait-based library could not be published to other Wado packages at all —
even though Wado→Wado linking shares Wasm GC types directly and never needs the
CM ABI (see [Package Manifest](./wep-2026-02-14-package-manifest.md)
§"Wado-to-Wado Optimization").

The naming also repeated a known Rust friction: `pub` named the in-library
module boundary, so the genuine library boundary had to borrow the same word.

## Decision

Split the single `export` ladder into two orthogonal axes.

Axis 1 — Wado visibility (a scope ladder):

| Keyword    | Reach                                 | Rust analogue |
| ---------- | ------------------------------------- | ------------- |
| (none)     | The defining file                     | (none)        |
| `internal` | Other files in the same package       | `pub(crate)`  |
| `pub`      | Other Wado packages (the library API) | `pub`         |

Axis 2 — CM surface (an orthogonal, additive flag):

| Keyword  | Meaning                                                            |
| -------- | ------------------------------------------------------------------ |
| `export` | Also lower this item at the CM boundary. Must be CM-representable. |

`export` is the analogue of Rust's `extern "C"` + `#[no_mangle]`: a separate
ABI surface, not a visibility level. Rules:

- `export ⟹ pub`. A CM export is by definition part of the public API. The
  former `pub export` collapses to plain `export`.
- `export` requires CM-representability, checked at the definition site (so the
  "appears in WIT" guarantee stays static). Closures, generics with non-WIT
  bounds, and `<effect E>` in an exported signature remain a compile error (see
  [WIT Interoperability](./wep-2026-05-02-wit-interoperability.md)).
- `pub` (without `export`) carries no CM restriction. Generic, higher-order,
  and trait-based items become publishable across the Wado library boundary.

```wado
fn tokenize(s: String) -> List<Token> { ... }              // file-private
internal fn build_ast(ts: List<Token>) -> Doc { ... }      // package-internal
pub fn map<T, U>(f: fn(T) -> U, xs: List<T>) -> List<U>    // library API (Wado-native)
export fn parse(s: String) -> Doc { ... }                  // library API + CM boundary
```

### Reach of `pub` vs `export`

A `pub`-only item is Wado-native: it reaches any Wado consumer (a source
dependency, or a `.wasm` with Wado provider metadata, via the GC-sharing path).
An `export` item additionally reaches any CM consumer through the Canonical
ABI. A standalone `.wasm` consumed by a non-Wado component therefore exposes
only `export` items — `pub`-only items are invisible across that boundary.
This follows directly from the producer/consumer matrix in
[Package Manifest](./wep-2026-02-14-package-manifest.md) §"Wado-to-Wado
Optimization"; the only change is that the library API is now `pub`, not
`export`.

### Why no `pub(crate)` / `pub(super)` family

Wado is flat (1 file = 1 module; a package is a set of modules with no nested
module privacy). A single `internal` covers every in-package case, so the
scope-parameterized `pub(...)` forms have nothing to scope to. Unlike Rust,
`pub` is absolute: a `pub` item is library-public, never gated by an enclosing
private module.

### Re-export visibility

The same ladder applies to re-exports. A `use` declaration may carry a
visibility modifier, re-exporting the imported names at that reach:

- `pub use { x } from "M"` — `x` joins this module's public API.
- `internal use { x } from "M"` — `x` is re-exported package-internal.
- plain `use { x } from "M"` — a file-private import; nothing is re-exported.

A re-export's reach is the re-export keyword's, **not** the original item's
visibility. So a `pub use` of an `internal` symbol publishes it: this is the
canonical "internal implementation, public facade" pattern, where a package's
entry module re-exports its internal submodules' items as the library API
(`core:prelude` and `core:kiln` are built this way — their submodule
definitions are `internal`, surfaced as `pub` through the facade's `pub use`).
The only constraint is that the re-export must itself be a legal import: you can
`pub use { x }` only a name visible at the re-export site (`x` is `pub`, or `x`
is `internal` and `M` is in this package). `wado doc` and the public-API query
view present such re-exports at the re-export's visibility, so a `pub use`-d
`internal` item appears as part of the facade module's public API.

## Consequences

- Generic / higher-order / trait libraries are publishable across packages
  (`pub`), which `export` could never express.
- `export` keeps its 1:1 correspondence with WIT `export`, with one job:
  the CM boundary.
- Migration (pre-stable, no shim): former `pub` → `internal`; former `export`
  and `pub export` → `export`; items that were `pub` only because they are a
  package's public API and were also `export`ed need no change. `pub use`
  re-exports gain an `internal use` counterpart for package-internal
  re-exports.

## Implementation

- [x] Parse `internal` (the keyword was previously reserved as a no-op).
      `internal` and `pub` are mutually exclusive; `export` implies `pub` (no
      `pub export`). The AST carries a `Visibility { Private, Internal, Public }`
      enum on every top-level declaration, orthogonal to the `is_export` flag.
- [x] Package identity: `ModuleSource::package_id()` groups modules into
      packages. `core:*` is one package, `wasi:*` another (independent), the entry
      point and its local modules the `Root` package, and each resolved dependency
      / remote URL its own package.
- [x] Enforcement at import resolution (analyze phase): file-private symbols are
      never importable; `internal` reaches only same-package importers; `pub` /
      `export` reach anywhere. A violation is a `PRIVATE_SYMBOL` compile error. The
      `Symbol` and re-export entries carry their declared visibility; namespace
      imports register only the visible members.
- [x] `internal use` re-exports (the package-internal counterpart to `pub use`).
- The bundled `core:internal` module was renamed to `core:rt` so the module
  name no longer collides with the `internal` visibility keyword.

## References

- [Package Manifest (`wado.toml`)](./wep-2026-02-14-package-manifest.md)
- [World Conformance and Export Syntax](./wep-2026-01-16-world-conformance-and-export.md)
- [WIT Interoperability](./wep-2026-05-02-wit-interoperability.md)
- [WIT and Wado Mapping](./wep-2026-01-29-wit-wado-mapping.md)
- [Re-export Syntax (`pub use`)](./wep-2026-01-25-pub-use-reexport.md)
