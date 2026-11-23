# WEP: Every Package Has a Coordinate

## Context

`[package].namespace` was optional. A package without one had no coordinate,
and the rules around packages grew a second case for it:

- Other packages could depend on it only through a `lib:` alias, since no
  coordinate key could name it
  ([Package and Module Specifier Syntax](./wep-2026-06-17-package-module-syntax.md)).
- A file it exports had no coordinate to be named under. Kiln keyed such a file
  as `name@ver/path`, a form with no `:`, which a local path could spell
  ([Package File Exports](./wep-2026-09-06-package-file-exports.md)).
- Building its library world as a component failed, since the default
  interface is named `<namespace>:<name>/<name>@<version>`.
- `wado publish` skipped it with "no namespace", beside `publish = false`.

The namespace was optional so that a package nobody publishes did not have to
pick one. `publish = false` already says that, so the optional namespace bought
nothing the second case did not cost.

## Decision

`[package].namespace` is required. A manifest whose `[package]` has none is an
error, as one without `name` or `version` is. A workspace member inherits the
namespace of `[workspace.package]`, as before.

Every package therefore has a coordinate, `namespace:name`, whether or not it is
published. What follows from that:

- A coordinate does not mean published. `publish = false` is the only way a
  package stays out of a registry, and publishing no longer checks the
  namespace.
- `wado init` takes `--namespace` as well as `--name`, and writes both.
- An exported file and a Kiln input from another package are always named
  `namespace:name@version/path`.
- A `lib:` alias renames a dependency, shortens its name, or tells two major
  versions apart. It is no longer the only way to depend on a package, since
  every package has a coordinate a key can be.

## Roadmap

1. [x] `wado-manifest` refuses a `[package]` with no `namespace`, and
       `wado init` requires `--namespace`.
2. [x] Every manifest in this repository declares a namespace or inherits one.
3. [x] The specification states the rule
       ([Packages](./spec-packages.md#package)).

## Known gaps

- A coordinate key with a `path` or `git` source is not checked against the
  dependency's own `[package]`. `"acme:geo" = { path = "../geo" }` builds
  whatever coordinate `../geo` declares.
- Two dependency keys can reach two different checkouts that declare the same
  coordinate. They are two packages, with two sets of types.
- A single-file `path` dependency (`path = "../shared.wado"`) has no manifest,
  so it has no namespace and no coordinate. Only a `lib:` key can name it.
- This repository's manifests still name sibling packages by `lib:` aliases
  that only shorten the coordinate (`"lib:gale"` for `wado-lang:gale`).
