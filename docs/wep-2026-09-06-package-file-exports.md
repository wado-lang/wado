# WEP: Package File Exports — Assets and Submodules as API

## Context

A Wado package ships more than its entry module. `package-gale-highlight-wado`
carries `grammar/Wado.g4` and `grammar/Wado.highlights.scm`; another package
carries a `.proto`, a JSON Schema, a `.css`, an icon. Inside the package those
files are ordinary Kiln inputs and `#include_str` arguments. Outside it they do
not exist:

- A Kiln `from` / `inputs` path must start with `./` or `../` and resolves
  against the declaring file ([Kiln](./wep-2026-04-12-kiln.md) §"Use-site
  syntax"). Nothing names a file inside a dependency.
- A module specifier names a package and resolves to its `[package].lib` entry;
  it "carries no interface segment"
  ([Package and Module Specifier Syntax](./wep-2026-06-17-package-module-syntax.md)),
  and no file segment either. A package's other `.wado` modules are unreachable
  the same way its `.g4` is.
- `#include_str` resolves relative to the including file
  ([Compile-Time File Inclusion](./wep-2026-03-02-include-str.md)).
- `wado publish` uploads a component and nothing else, so for a registry
  dependency there is no file on disk to reach even if the syntax existed.

A package cannot say which of its files it offers, and a consumer cannot name
one. Both are needed, and they have to arrive together: a file reference looks
like a filesystem path, and a package's directory layout must not become its API
by accident.

## Decision

### One allowlist, both kinds of file

A package declares the files a consumer may name. Everything else is invisible,
whatever the directory contains.

```toml
[package]
name = "gale-highlight-wado"
lib = "src/lib.wado"
exports = [
    "grammar/Wado.g4",
    "grammar/Wado.highlights.scm",
    "src/highlight.wado",
]
```

One list covers assets and `.wado` submodules alike. What differs between them
is what a consumer does with the file: import it as a module, feed it to a
generator, inline it. What the package promises is the same either way, and
deciding it is the owner's, as it is for `pub` and `export` on an item.

A refusal names the package, not the path. Reaching for an unlisted file answers
"package `X` exports no file `Y`" whether or not the file is there, so the list
never doubles as a directory listing.

### Naming an exported file

```
"<package-specifier>/<path>"
```

The path is relative to the package root (the directory holding its
`wado.toml`), forward-slash separated, and carries its extension:

```wado
use { highlight } from "wado-lang:gale-highlight-wado/src/highlight.wado";

use { Parser } from "lib:gale-highlight-wado/grammar/Wado.g4" with {
    generator: { module: "lib:gale" },
};

let template = #include_str("wado-lang:my-pkg/templates/index.html");
```

- A specifier with no path segment keeps its current meaning: the package's
  `[package].lib` entry. The extension separates the two forms, so it is
  required rather than conventional. It also keeps a file reference from reading
  like a WIT interface segment.
- Every specifier form that can carry a package coordinate carries a path the
  same way: an open coordinate (`ns:name[@ver]`) and a `lib:` nickname alike.
- `..` and absolute paths are rejected, not resolved. The path is a key into the
  allowlist, not a traversal that happens to start at the package root.
- The reference resolves against the consuming manifest's own `[dependencies]`
  / `[build-dependencies]`, like every other specifier. There is no path into a
  transitive dependency.

This revises §"Specifier forms" of
[Package and Module Specifier Syntax](./wep-2026-06-17-package-module-syntax.md):
a specifier names a package, optionally followed by one exported file. Its
prohibition is on interface segments, and that one stands. An interface is
selected in the `use { … }` list, and a file is not an interface.

### The allowlist is API

- Adding an entry is additive. Removing or renaming one breaks consumers, like
  removing an `export fn`.
- `wado publish` ships every listed file, and refuses a list naming a file the
  package does not carry. The publisher sees that failure, instead of a consumer
  months later.
- A source dependency (path / git) serves the files from its checkout. A
  registry dependency carries them in the `wado:package` section
  ([Provider Metadata](./wep-2026-07-26-provider-metadata.md)), which grows an
  asset area beside the `.wado` sources it already carries. Consumer selection
  stays all-or-nothing: a dependency consumed through the CM path has no files
  to offer, and says so.

### Kiln

- `from` and `inputs` accept an exported-file reference wherever they accept a
  `./` path today.
- The invocation's identity records the logical reference
  (`ns:name@ver/path`), never where the file sits on this machine. That covers
  the cache key, the dedup tuple, and the `sources` of the `#![generated]`
  header. A dependency's checkout lives under `$WADO_ROOT` at a
  version-dependent path, so hashing that path would make the cache
  machine-dependent and break the committed-cache workflow
  ([Kiln](./wep-2026-04-12-kiln.md) §"Caching").
- Generated files land in the consuming project's output tree, exactly as they
  do for a local schema. A dependency's checkout is shared between projects and
  is never written to.
- Kiln's design principles hold unchanged. Every input is still enumerated
  literally at the use site (principle 2), since a package coordinate in front
  of the path does not make the set dynamic, and the generator still receives
  its inputs by value under the same sandbox (principle 1).
- The clause stays the consumer's. Naming a dependency's `.g4` means running the
  generator yourself, with your options. A package that wants to own its own
  generation instead runs Kiln on its own files and exports the resulting
  module, which needs the harvest to cross the dependency edge (see Known gaps).

### Deliberate omissions

Globs at a use site and directory listing, both already refused for local Kiln
inputs: a package boundary is not the place to relax them. Writing into a
dependency. Reaching a file of a dependency's dependency.

## Roadmap

1. `[package].exports` in `wado-manifest`: parse, and validate each entry as a
   package-root-relative path with an extension and no `..`. Done when a
   manifest round-trips it and `wado publish` refuses a list naming a file the
   package does not carry.
2. Specifier resolution: split `<coordinate>/<path.ext>` once (`kiln::parse_spec`
   already separates the segment), resolve the package through the dependency
   index, check the allowlist, and refuse at package granularity. Done when
   `use { … } from "lib:x/src/y.wado"` compiles against a path dependency and an
   unlisted path produces the package-level diagnostic.
3. Kiln `from` / `inputs`: accept the reference, key the cache and the header on
   the logical form, write outputs into the consumer's tree. Done when
   `grammar/Wado.g4` can be consumed from a second package with a warm,
   machine-independent cache.
4. `#include_str` / `#include_bytes`: the same reference, resolved through the
   same path.
5. Registry distribution: assets travel in the `wado:package` section and are
   extracted into the shared cache on fetch, so `CompilerHost::load_source`
   serves them like any other file.
6. Binary assets through Kiln: `input-file.content` is a `string`, so an image
   cannot cross the generator boundary. Done when the `core:kiln` world carries
   bytes and its version bump invalidates the caches that need it.

## Known gaps

- The key's name. `exports` reads as "part of the API", but `export` already
  means "crosses the CM ABI" on an item, and these files cross nothing. `public`
  and `files` are the alternatives.
- Glob patterns in the allowlist (`grammar/*.g4`). Exact entries are what a use
  site needs and what publish can verify; a package with many assets will want
  patterns. Closing it means saying what a pattern means for the stability
  promise and for what publish uploads.
- A dependency's own Kiln invocations. The clause-harvest walks the local module
  graph and stops at the dependency edge, so a package that generates from its
  own assets does not work when consumed. Closing it means harvesting through
  dependency modules and anchoring their invocation paths and outputs in the
  consuming project.
- Whether an exported submodule may offer items its `lib` entry does not, and
  how that interacts with `pub` / `export`
  ([Visibility](./wep-2026-06-25-visibility-internal-pub-export.md)).
- Reserved namespaces (`core:` / `wasi:` / `web:`) expose no files. Whether a
  bundled asset should ever be nameable is unexamined.
- Case-insensitive and Unicode-normalizing filesystems: the allowlist is
  compared as bytes, and a path that matches on one machine may not on another.

## References

- [Kiln — Keyed IDL Lowering Notation](./wep-2026-04-12-kiln.md)
- [Package and Module Specifier Syntax](./wep-2026-06-17-package-module-syntax.md)
- [Package Manifest (`wado.toml`)](./wep-2026-02-14-package-manifest.md)
- [Provider Metadata — Source-Bundled Package Artifacts](./wep-2026-07-26-provider-metadata.md)
- [Compile-Time File Inclusion (`#include_str`)](./wep-2026-03-02-include-str.md)
- [Visibility — `internal` / `pub` / `export`](./wep-2026-06-25-visibility-internal-pub-export.md)
