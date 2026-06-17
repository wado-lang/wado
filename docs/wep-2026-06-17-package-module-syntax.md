# WEP: Package and Module Specifier Syntax

## Context

The module specifier (`from "..."`) carried two unreconciled designs:

- [Module Loader](./wep-2026-01-24-module-loader.md) fixed the namespace set to
  `core` and `wasi` and rejected any other `ns:` as an unknown namespace.
- [Package Manifest](./wep-2026-02-14-package-manifest.md) added bare-name
  dependency aliases (`use { R } from "router"`) resolved through `wado.toml`.

Neither lets a specifier name a Component Model package by its real registry
coordinate (`ns:pkg@ver`), so the WIT identity a Wado package already has does
not show through in source — counter to "Wasm in plain sight". Bare names are
also ambiguous on sight (`"router"` reads like a mistyped path), and a
single-file script has no way to declare an external dependency at all.

This WEP settles the specifier grammar and the resolution rules behind it. It
supersedes the namespace grammar of Module Loader and the bare-name resolution
of Package Manifest; the latter is updated in place as a living spec.

## Decision

A specifier is a CM package coordinate, a `lib:` alias, a local path, or a
remote URL. Two principles anchor the rest:

- **Reserved namespace ⇔ bundled namespace.** A namespace is reserved iff the
  compiler ships its bytes — today `wasi` and `core`. Every other namespace is
  open and resolves from outside. `lib` is the one extra reserved namespace, and
  carries a single responsibility (below).
- **Manifest key ≡ specifier string.** A `[dependencies]` key is byte-identical
  to the `from "..."` string it backs. There is no key-level indirection.

### Specifier forms

| Form                             | Resolver                                      |
| -------------------------------- | --------------------------------------------- |
| `wasi:…` / `core:…`              | Reserved → compiler-bundled                   |
| `ns:pkg[/iface][@ver]` (open ns) | Default registry, or `with`/manifest override |
| `lib:nick`                       | Indirection: alias / rename / private dep     |
| `./` `../`                       | Local file                                    |
| `http(s)://`                     | Remote                                        |

`wasi:`/`core:` are not a separate scheme — they are coordinates whose
namespace happens to be bundled. Nested namespaces (`a:b:pkg`) and `/iface`
follow WIT.

### `lib:` is the sole home for indirection

Renames, short names, multiple coexisting majors, and dependencies that have no
public coordinate (closed-source git/path, the natural state of an
unpublishable package) all use `lib:nick`. Aliases are forbidden under any open
`ns:`: that keeps a real coordinate (`from "foo:regexp"`, transparent) visually
distinct from a local indirection (`from "lib:rx"`, "see the manifest").

The `package` field bridges a `lib:` alias to its real coordinate:

| Key (≡ specifier) | `package` | Source                                    |
| ----------------- | --------- | ----------------------------------------- |
| `"foo:regexp"`    | omitted   | key is the coordinate                     |
| `"lib:rx"`        | required  | registry alias → `package = "foo:regexp"` |
| `"lib:shared"`    | optional  | git/path; resolved package self-declares  |

Multiple majors fall out of this without special keys:

```toml
"lib:http1" = { package = "std:http", version = "^1.0" }
"lib:http2" = { package = "std:http", version = "^2.0" }
```

### Version: a range only where a lock backs it

| Position                             | Form                                       |
| ------------------------------------ | ------------------------------------------ |
| Manifest `version`                   | range required (`^`/`~`/`=`); bare = error |
| Specifier `@ver`, single-file `with` | exact only; a range here is an error       |

A range is meaningful only when a `wado.lock` resolves it. Lock-less positions
take an exact pin; a range there is reported with a clear message (use an exact
version, or declare the dependency in `wado.toml`).

### Single-file parity

With no `wado.toml`, an inline `with { ... }` carries the same source vocabulary
as a `[dependencies]` value (`git`/`ref`/`registry`/`package`/`path`/exact
`version`). Inline `with` and a manifest entry for the same specifier are
mutually exclusive. Single-file scripts have no lock file — reproducibility
comes from the exact pins in `with`.

Bare names (`from "router"`) are rejected everywhere.

## Consequences

### Positive

- A specifier self-describes: a coordinate is a transparent CM identity; `lib:`
  is the visible mark of indirection. Nothing is silently rewritten.
- The Module Loader / Package Manifest contradiction is gone: reserved = bundled
  is a single, non-arbitrary rule.
- Single-file scripts gain the manifest's full expressive power inline.
- Coverage is complete with one indirection point: public-transparent (direct
  coordinate), rename/short/multi-major/coordinate-less (`lib:`), bundled
  (`wasi:`/`core:`), local (`./`), remote (`http(s)://`).

### Trade-offs

- `lib` is reserved without being bundled — the one exception to reserved =
  bundled, justified by its single responsibility (all indirection lives there).
- A git-hosted package with a coordinate can be reached two ways (direct
  coordinate + `with { git }`, or `lib:nick` + `package` + `git`). Accepted; it
  mirrors Cargo's rename-vs-direct duality.
- Requiring a namespace on every external coordinate is more typing than a bare
  name, but it is the transparent, unambiguous form and follows from the
  bare-name ban.
