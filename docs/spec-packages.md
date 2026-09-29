# Packages

A package is a set of modules that is built, versioned and depended on as one
unit. This chapter covers which modules make up a package, the manifest that
describes it, the dependencies it declares, and what it offers the packages that
depend on it.

## The Modules of a Package

A package bounds how far `internal` reaches
([Visibility](./spec-modules.md#visibility)) and which impls it may write
([Package Boundary](./spec-traits.md#package-boundary)). A program's modules
fall into packages this way:

- The entry module, every local module it reaches through `./` / `../`
  imports, and the Wasm assets those modules import form one package.
- Each dependency is a package. A relative import inside a dependency stays in
  that dependency's package.
- `core:*`, which is one package, and `wasi:*`, which is another. A test of
  the standard library is an entry module in its `core` directory, and it
  joins `core:*`'s package together with its local modules, so it reaches
  their `internal` items.
- A [generated module](./spec-kiln.md) belongs to the package of the
  module that imports it.

## The Manifest

A package's manifest is the file `wado.toml` in its root directory. Every path
the manifest holds is relative to that directory. This is the manifest of
`package-gale-highlight-wado`:

```toml
[package]
name = "gale-highlight-wado"
description = "Syntax highlighter for the Wado language, built on Gale"
repository-directory = "package-gale-highlight-wado"
lib = "src/lib.wado"
# version / repository / namespace / license / authors inherited from [workspace.package]

[world]
"wasi:cli/command" = "src/main.wado"

# gale is used in two roles: the Kiln generator that turns the bundled grammar
# into a parser at build time, and the `gale-highlight` runtime lib (`run_cli`).
[build-dependencies]
"lib:gale" = { path = "../package-gale", package = "wado-lang:gale", version = "^0.0.19" }

[dependencies]
"lib:gale" = { path = "../package-gale", package = "wado-lang:gale", version = "^0.0.19" }

[test]
# `build/` holds Kiln intermediates; not source (see package-gale's note).
exclude = ["build/**"]
```

The manifest holds these tables:

| Table                  | Holds                                                | Specified in                                            |
| ---------------------- | ---------------------------------------------------- | ------------------------------------------------------- |
| `[package]`            | The package's identity and metadata, and its library | [`[package]`](#package)                                 |
| `[world]`              | The hosted worlds the package targets                | [Selecting a World](./spec-worlds.md#selecting-a-world) |
| `[dependencies]`       | The packages its code imports                        | [Dependencies](#dependencies)                           |
| `[dev-dependencies]`   | The packages only its development needs              | [Dependencies](#dependencies)                           |
| `[build-dependencies]` | The Kiln generators its imports run                  | [Kiln Generators](./spec-kiln.md#manifest)              |
| `[registries]`         | Names for the registries its dependencies come from  | [Registries](#registries)                               |
| `[workspace]`          | The packages developed together with it              | [Workspaces](#workspaces)                               |
| `[test]`               | Which files `wado test` discovers                    | [Test Discovery](./spec-testing.md#test-discovery)      |
| `[format]`             | Which files `wado format` discovers                  | [Wado Formatter](./formatter.md)                        |

A key the manifest does not define draws a warning and is ignored. This holds at
the top level, in `[package]`, in `[workspace.package]`, and in the table form of
a `[world]` entry. A misspelled key is reported, and the build goes on. Every
other mistake in a manifest is an error.

```toml
[package]
name = "geo"
version = "0.2.0"
descripton = "Plane geometry"   # warning: unknown field "descripton" in [package] (ignored)
```

A file with no `wado.toml` above it is compiled on its own. It can still name a
dependency, through an inline source on the `use`
([Inline Sources](#inline-sources)).

## `[package]`

`[package]` names the package and describes it.

```toml
[package]
namespace = "acme"
name = "geo"
version = "0.2.0"
lib = "src/lib.wado"
description = "Plane geometry"
homepage = "https://geo.acme.example"
repository = "https://github.com/acme/tools"
repository-directory = "packages/geo"
documentation = "https://docs.acme.example/geo"
license = "MIT OR Apache-2.0"
authors = ["Alice <alice@acme.example>"]
wado-version = ">=0.5"
```

| Field                  | Type     | Required | Meaning                                                            |
| ---------------------- | -------- | -------- | ------------------------------------------------------------------ |
| `name`                 | string   | yes      | The package's name                                                 |
| `version`              | string   | yes      | The package's version, in semantic versioning (`0.2.0`)            |
| `namespace`            | string   | no       | The organization or person the package belongs to                  |
| `lib`                  | string   | no       | The entry module of the package's library world                    |
| `description`          | string   | no       | A short summary                                                    |
| `homepage`             | string   | no       | The home page URL; `repository` when absent                        |
| `repository`           | string   | no       | The source repository URL, without a subdirectory                  |
| `repository-directory` | string   | no       | The package's directory inside `repository`                        |
| `documentation`        | string   | no       | The documentation URL; `repository` when absent                    |
| `license`              | string   | no       | An SPDX license expression                                         |
| `license-file`         | string   | no       | A file holding a license that has no SPDX identifier               |
| `authors`              | string[] | no       | The people or organization responsible                             |
| `wado-version`         | string   | no       | The compiler versions that can build the package (`>=0.5`)         |
| `publish`              | bool     | no       | `false` keeps the package from being published; `true` when absent |

`name` and `namespace` are each 1 to 64 characters from `[a-zA-Z0-9_-]`. The
two together, `namespace:name`, are the package's coordinate: its identity in a
registry, and the specifier other packages import it by.

```toml
[package]
namespace = "acme"   # coordinate: acme:geo
name = "geo"
```

A package with no `namespace` has no coordinate. It cannot be published, which
suits an application or an internal tool. Other packages can still depend on it
through a `path` or `git` source and a `lib:` key.

`version` is a full semantic version, so `0.1` is an error. `wado-version` is a
semantic-versioning requirement, such as `>=0.5`.

`license` is checked as an SPDX expression, and one that does not parse is an
error. `license` and `license-file` exclude each other.

```toml
[package]
name = "geo"
version = "0.2.0"
license = "MIT"
license-file = "LICENSE.txt"   # error: `license` and `license-file` are mutually exclusive
```

`lib` names the library world's entry module. That module is what the package
offers other packages ([The Library World](#the-library-world)). A package
declares at least one world ([Selecting a World](./spec-worlds.md#selecting-a-world)).

## Dependencies

A dependency is another package that this one uses. Three tables declare them,
all with the same form:

- `[dependencies]` holds the packages the package's code imports with `use`.
- `[dev-dependencies]` holds the packages only its development needs. They are
  resolved and locked like the others, and the lock file marks them `dev`.
- `[build-dependencies]` holds the Kiln generators its imports run
  ([Kiln Generators](./spec-kiln.md#manifest)).

A key is a coordinate or a `lib:` alias, and
[Package Specifiers](#package-specifiers) says how a `use` names it. Each part
of a key between colons is 1 to 64 characters from `[a-zA-Z0-9_-]`.

```toml
[dependencies]
"acme:geo" = { path = "../geo" }             # a coordinate
"lib:geo" = { path = "../geo" }              # an alias
"lib:geo.v2" = { path = "../geo-next" }      # error: invalid dependency key "geo.v2"
```

Each value is an inline table that gives the dependency's source: where its code
comes from. A source is a local path, a git repository, or a registry.

### Path Sources

A `path` source names a directory on the local file system, or a single `.wado`
file.

```toml
[dependencies]
"lib:geo" = { path = "../geo" }          # a directory with its own wado.toml
"lib:shared" = { path = "../shared.wado" }   # one file
```

A directory must hold a `wado.toml` whose `[package].lib` names the entry module
([The Library World](#the-library-world)). A single `.wado` file is its own
entry module. A path dependency is read from disk on every build and is never
locked.

A path source may also carry a git or registry source. The build uses the path,
and ignores the other source. Publishing replaces the path with the other
source, so the published package does not point at a directory on the
publisher's machine ([Publishing](#publishing)).

```toml
[dependencies]
# Built from ../package-gale; published as a dependency on wado-lang:gale ^0.0.19.
"lib:gale" = { path = "../package-gale", package = "wado-lang:gale", version = "^0.0.19" }
# Built from ../shared; published as a dependency on the git repository.
"lib:shared" = { path = "../shared", git = "https://github.com/acme/shared.git", version = "^0.1.0" }
```

### Git Sources

A `git` source names a repository by its URL, on any host.

| Field       | Required     | Meaning                                                           |
| ----------- | ------------ | ----------------------------------------------------------------- |
| `git`       | yes          | The repository URL                                                |
| `version`   | one of these | A version requirement, matched against the repository's tags      |
| `ref`       | one of these | A tag, a branch or a commit SHA                                   |
| `directory` | no           | The package's directory inside the repository; the root if absent |

A git source takes exactly one of `version` and `ref`. Giving both, or neither,
is an error.

```toml
[dependencies]
"lib:router" = { git = "https://github.com/user/router.git", version = "^1.0.0" }
"lib:router-next" = { git = "https://github.com/user/router.git", ref = "main" }
"lib:foo" = { git = "https://github.com/org/monorepo.git", version = "^1.0.0", directory = "packages/foo" }
"lib:bad" = { git = "https://github.com/user/bad.git", version = "^1.0.0", ref = "main" }   # error
```

A `version` is matched against the tags that name a version. Leading letters are
dropped from a tag before it is read, so `v1.2.0` and `release1.2.0` both name
`1.2.0`. A tag that is not a version after that is ignored.

`directory` is accepted only beside `git`. A path source names its directory
directly.

### Registry Sources

A registry source has a `version` and no `path` or `git`. The package comes from
a registry, by its coordinate.

| Field      | Required         | Meaning                                            |
| ---------- | ---------------- | -------------------------------------------------- |
| `version`  | yes              | A version requirement                              |
| `package`  | for a `lib:` key | The coordinate of the package the alias stands for |
| `registry` | no               | A name from `[registries]`; `default` when absent  |

A coordinate key is the package's own coordinate, so it takes no `package`. A
`lib:` key names the coordinate it stands for with `package`. A `lib:` key with
neither `package` nor another source is an error, since nothing says which
package it is.

```toml
[registries]
default = "oci://ghcr.io"
acme = "oci://registry.acme.example/wado"

[dependencies]
"wado-lang:cm-catalog" = { version = "^0.1.0" }                         # default registry
"lib:catalog" = { package = "wado-lang:cm-catalog", version = "^0.1.0" }  # an alias of it
"lib:geo" = { registry = "acme", package = "acme:geo", version = "^0.2.0" }
"lib:nothing" = { version = "^1.0.0" }                                  # error: which package?
```

A registry source with no `registry` field needs `[registries].default`, and
without one it is an error.

### Version Requirements

A `version` in the manifest is a requirement: the range of versions the
dependency may resolve to. It starts with one of three operators, and a version
with no operator is an error.

| Requirement | Meaning                | Accepts           |
| ----------- | ---------------------- | ----------------- |
| `^1.2.3`    | compatible             | `>=1.2.3, <2.0.0` |
| `^0.2.3`    | compatible, before 1.0 | `>=0.2.3, <0.3.0` |
| `^0.0.3`    | compatible, before 0.1 | `>=0.0.3, <0.0.4` |
| `~1.2.3`    | patch updates only     | `>=1.2.3, <1.3.0` |
| `=1.2.3`    | exactly this version   | `1.2.3`           |

```toml
[dependencies]
"acme:geo" = { version = "^0.2.0" }   # OK
"acme:ui" = { version = "0.2.0" }     # error: bare version "0.2.0" requires explicit prefix (^, ~, or =)
```

Resolution picks, for each dependency, the highest version that meets every
requirement on it anywhere in the dependency graph. When no version meets them
all, resolution fails and says which requirements conflict.

A range is allowed only in `wado.toml`, where a lock file resolves it. A
specifier's `@version` and an inline source take an exact version, and a range
there is an error.

## Package Specifiers

A `use` names a dependency by a coordinate `<ns>:<pkg>` in an open namespace,
or by an alias `lib:<nick>`
([Module Source Types](./spec-modules.md#module-source-types)). Either is
resolved from the `[dependencies]` entry whose key is byte-identical to the
specifier, or from an inline source on the `use`. An undeclared coordinate or
alias is an error.

A key under an open namespace is the dependency's own coordinate. `lib` is the
one place an alias lives. An alias renames a dependency, shortens its name,
tells two major versions apart, or names a dependency with no public
coordinate. A `lib:` key names the coordinate it stands for with its `package`
field ([Registry Sources](#registry-sources)).

```toml
[dependencies]
"docs:regex" = { version = "^0.1.0" }                            # use { … } from "docs:regex"
"lib:rx" = { package = "docs:regex", version = "^0.1.0" }        # use { … } from "lib:rx"
"lib:shared" = { path = "../shared" }                            # use { … } from "lib:shared"
"router" = { path = "../router" }                                # warning: bare name (deprecated)
```

A bare key (`"router"`) is deprecated and draws a warning. It is the one bare
name a `use` may name.

### Inline Sources

An inline dependency source lets a single-file script name a dependency with no
`wado.toml`:

<!-- {"source": "wado-cli/tests/fixtures/inline_dependencies.wado"} -->

```wado
use { Regexp } from "docs:regex@1.0.0" with { registry: "oci://ghcr.io/acme" };  // exact pin via the specifier
use { Router } from "lib:router" with { git: "https://github.com/user/router.git", ref: "v1.0" };
use { Parse } from "lib:rx" with { registry: "oci://ghcr.io/acme", package: "docs:regex", version: "1.0.0" };
```

An inline source takes the same keys as a `[dependencies]` value: `git`, `ref`,
`directory`, `registry`, `package`, `path`, and an exact `version`. An inline
source and a `wado.toml` entry for the same specifier are mutually exclusive.

## Registries

`[registries]` gives registry URLs short names. A registry source picks one with
its `registry` field, and `default` is the one a source without the field uses.

```toml
[registries]
default = "oci://ghcr.io"
acme = "oci://registry.acme.example/wado"
```

A registry is an OCI registry, written `oci://<host>[/<prefix>]`. The package
`namespace:name` at version `x.y.z` is the image `<host>/<prefix>/<namespace>/<name>`
tagged `x.y.z`. With the `default` above, `wado-lang:cm-catalog` version `0.1.0`
is `ghcr.io/wado-lang/cm-catalog:0.1.0`. That image holds the library world. Each
other world a package publishes is its own image, one path segment further down,
named after the world with `:` and `/` written as `-`:
`ghcr.io/wado-lang/gale/core-kiln-generator:0.0.9`.

A registry package is a prebuilt component. Its specifier imports the
interfaces the component exports, as a `use` of a component file does
([Components](./spec-modules.md#components)). A `pub` item the component does
not export does not reach the consumer. Whether a registry package will also
carry its `pub` items to a Wado consumer is undecided
([WEP: Provider Metadata](./wep-2026-07-26-provider-metadata.md)):

<!-- {"source": "example/hello-packages/src/main.wado"} -->

```wado
use { println, Stdout } from "core:cli";
use { CmCatalog } from "wado-lang:cm-catalog";
```

<!-- {"source": "example/hello-packages/src/main.wado"} -->

```wado
let n = CmCatalog::id_u32(42);
let s = CmCatalog::id_string("hello");
```

## The Library World

A package offers other packages its library world
([What is a World?](./spec-worlds.md#what-is-a-world)). The world's entry module
is the file `[package].lib` names, and a specifier that names the package
imports that module. A dependency without `[package].lib` cannot be imported,
unless it is a single `.wado` file named by a path source, which is its own
entry module.

These two files are a package `geo`, with `lib = "src/lib.wado"`:

<!-- {"fixture": "sub/spec_packages_geo/lib.wado"} -->

```wado
// geo/src/lib.wado - the file `[package].lib` names
pub use { Square } from "./shapes.wado";

pub fn area(s: Square) -> i32 { return s.side * s.side; }

pub fn map_sides<T>(squares: List<Square>, f: fn(i32) -> T) -> List<T> {
    return squares.into_iter().map(|s| f(s.side)).collect();
}

export fn unit_side() -> i32 { return 1; }

internal fn scale() -> i32 { return 2; }

fn margin() -> i32 { return 0; }

pub fn grown(s: Square) -> Square { return Square { side: s.side * scale() + margin() }; }
```

<!-- {"fixture": "sub/spec_packages_geo/shapes.wado"} -->

```wado
// geo/src/shapes.wado - a file the entry module re-exports from
pub struct Square { pub side: i32 }

pub fn perimeter(s: Square) -> i32 { return s.side * 4; }
```

### What a Dependency Offers

A package that depends on `geo` may import the entry module's `pub` and `export`
items, and the names it re-exports with `pub use`
([Visibility](./spec-modules.md#visibility)):

<!-- {"fixture": "spec_packages_library_api.wado"} -->

```wado
// app/src/main.wado - `"lib:geo" = { path = "../geo" }` in app/wado.toml
use { Square, area, grown, map_sides, unit_side } from "lib:geo";

test {
    let s = Square { side: 3 };
    assert area(s) == 9 && grown(s).side == 6;
    assert map_sides([s, Square { side: unit_side() }], |n| n * 10) == [30, 10];
}
```

An `internal` item stays inside `geo`, and a private one inside its file. Naming
either from another package is a compile error:

<!-- {"fixture": "spec_packages_internal_err.wado"} -->

```wado
use { scale } from "lib:geo";   // error: `internal` to the geo package
```

<!-- {"fixture": "spec_packages_private_err.wado"} -->

```wado
use { margin } from "lib:geo";   // error: private to geo's entry module
```

The specifier reaches the entry module and nothing else. A `pub` item of another
file in the package reaches a consumer only when the entry module re-exports it.
`perimeter` is `pub` in `shapes.wado`, but `lib.wado` does not re-export it:

<!-- {"fixture": "spec_packages_other_file_err.wado"} -->

```wado
use { Square, perimeter } from "lib:geo";   // error: `perimeter` is `pub` in a file the entry does not re-export
```

### Source Dependencies

A `path` or `git` dependency is Wado source. It is compiled into the consuming
program, and its `pub` items cross as Wado declarations. A `pub` signature may
therefore take a closure, a generic or a trait, which no component interface can
carry. `map_sides` above is one. So is the `run_cli` that
`package-gale-highlight-wado` imports from its `lib:gale` path dependency:

<!-- {"source": "package-gale-highlight-wado/src/main.wado"} -->

```wado
use { highlight } from "./lib.wado";
use { run_cli } from "lib:gale";
```

<!-- {"source": "package-gale-highlight-wado/src/main.wado"} -->

```wado
export fn run() with (Stderr, Environment, Preopens, MonotonicClock) {
    run_cli(|src: &String| highlight(*src), args());
}
```

A registry dependency is a component, so a consumer reaches only what the
component exports ([Registries](#registries)).

Whatever its source, a dependency's types and traits are foreign to the
consumer when the orphan rule asks
([Package Boundary](./spec-traits.md#package-boundary)).

### One Package, One Set of Types

A package is compiled once into a program, however many keys name it. Two keys
whose sources are the same package reach the same declarations, so a type
imported through one is the type imported through the other:

<!-- {"fixture": "spec_packages_one_package.wado"} -->

```wado
// Two [dependencies] keys whose sources are the same package
use { area } from "lib:geo";
use { Square } from "acme:geo";

test {
    assert area(Square { side: 2 }) == 4;   // one package, so one `Square`
}
```

A consumer's `[dependencies]` do not reach into its dependencies. A `use` inside
a dependency never resolves against them.

### The Library World as a Component

Built on its own, the library world is a component. Its interface carries the
entry module's `export` items and no `pub`-only item. The interface is named
after the package, `<namespace>:<name>/<name>@<version>`, so building it needs
`[package].namespace`. The world itself is named `root`, so no package may take
that name, in any letter case. An entry module that exports nothing gives the
component no interface, and building it is an error.

## The Lock File

`wado.lock`, beside `wado.toml`, records the exact version each registry and
git dependency resolved to. A build uses the versions the lock file records.
Committing it makes every build of the package use the same dependencies. This
is the lock file of `example/hello-packages`:

```toml
# This file is auto-generated by wado. Do not edit manually.
version = 1
deps-hash = "sha256:370208cbcf9701ecfad219813fade7ae411ef3661b7696a2a276d38c8912304f"

[[package]]
id = "registry+oci://ghcr.io/wado-lang:cm-catalog"
version = "0.1.0"
integrity = "sha256:e477653fa5dd32e208697f5e07464bab0bfb466a32bf5f0ed84e20ce9aa77d45"
deps = []

[[build-dependency]]
id = "registry+oci://ghcr.io/wado-lang:gale"
version = "0.0.9"
integrity = "sha256:1a49e9adc93a919e40c40fc155d2162061ece67bcc85b371fb1336995dd84ba0"
world = { "core:kiln/generator" = "" }
deps = []
```

The header holds two fields:

| Field       | Meaning                                                                                 |
| ----------- | --------------------------------------------------------------------------------------- |
| `version`   | The lock file format. It is `1`, and any other value is an error.                       |
| `deps-hash` | A hash of `[dependencies]`, `[dev-dependencies]` and `[build-dependencies]`, `sha256:…` |

Each `[[package]]` entry is one resolved package, direct or transitive. Each
`[[build-dependency]]` entry is one resolved generator, in the same form:

| Field          | Present for           | Meaning                                                      |
| -------------- | --------------------- | ------------------------------------------------------------ |
| `id`           | every entry           | The package's source and identity                            |
| `version`      | every entry           | The exact version                                            |
| `resolved-ref` | a git package         | The commit SHA the version resolved to                       |
| `integrity`    | a registry package    | The digest of the downloaded package, as `sha256:<hex>`      |
| `dev`          | a development package | `true` when only `[dev-dependencies]` reach the package      |
| `world`        | when it has any       | The package's `[world]` table, as an inline table            |
| `deps`         | every entry           | The entries the package depends on, each as `<id>@<version>` |

An `id` is `registry+<registry URL>/<coordinate>` for a registry package and
`git+<repository URL>/<key>` for a git one. The same coordinate in two
registries is two packages. Entries are sorted by `id`, then by `version`.

A path dependency has no entry.

## Workspaces

A workspace is a set of packages developed together. Its root `wado.toml` has a
`[workspace]` table, whose `members` lists the member directories as glob
patterns:

```toml
[workspace]
members = ["packages/*"]

[workspace.package]
version = "1.2.0"
repository = "https://github.com/example/tools"
namespace = "example"
license = "MIT"
authors = ["Alice <alice@example.com>"]
```

`[workspace.package]` holds metadata every member inherits, with no marker in
the member. A member inherits the fields in one of two ways:

| Field          | Inheritance                                              |
| -------------- | -------------------------------------------------------- |
| `version`      | Forced: a member that sets the field is an error         |
| `repository`   | Forced                                                   |
| `namespace`    | Forced                                                   |
| `license`      | A default the member may replace                         |
| `license-file` | A default the member may replace; resolved from the root |
| `authors`      | A default the member may replace                         |
| `wado-version` | A default the member may replace                         |

Forcing the identity fields keeps every member on one version, from one
repository, under one namespace. `license` and `license-file` are one setting: a
member that sets either replaces both.

```toml
# package-grog/wado.toml, a member of the workspace above
[package]
name = "grog"
version = "0.1.0"   # error: [package].version is inherited from [workspace.package]; remove it from this member
lib = "src/lib.wado"
```

Any other `[package]` field belongs to the member. A key in `[workspace.package]`
that is not one of the seven draws the unknown-key warning. The root may also
have a `[package]` of its own, which inherits nothing.

## Publishing

Publishing puts a package in a registry, under its coordinate. A package is
publishable when all of these hold:

- `[package]` has a `namespace`, so the package has a coordinate.
- `[package].publish` is not `false`.
- `[package]` has a `description`, a `repository` and at least one entry in
  `authors`.
- `[package]` has a `license` or a `license-file`.
- Every `[dependencies]` and `[build-dependencies]` entry has a source that
  another machine can reach. A path source needs a git or registry source beside
  it, which replaces it in the published package.

Each world a package declares is published as its own component
([Registries](#registries)). A `[world]` entry in table form keeps one world
back:

```toml
[world]
"wasi:cli/command" = "src/main.wado"                                 # published
"core:kiln/generator" = "src/generator.wado"                         # published
"wasi:http/service" = { entry = "src/server.wado", publish = false } # not published
```

`[package].publish = false` keeps the whole package back, whatever its worlds
say. A package with no publishable world has nothing to publish, which is an
error.

## Tools

The `wado` command edits and reads the manifest and the lock file. `wado init`
writes a new manifest. `wado update` resolves the dependencies and writes
`wado.lock`. `wado fetch` downloads what the lock file records. `wado publish`
checks the rules in [Publishing](#publishing) and uploads each publishable world.

Rationale: [WEP: Package Manifest](./wep-2026-02-14-package-manifest.md) and
[WEP: Package and Module Specifier Syntax](./wep-2026-06-17-package-module-syntax.md).
