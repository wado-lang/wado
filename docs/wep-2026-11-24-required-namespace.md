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
nothing, and cost a second case in each rule above.

With the second case gone, a coordinate key still trusted its source: nothing
checked that `"acme:geo" = { path = "../geo" }` reaches `acme:geo`, and two
directories declaring one coordinate compiled as two packages with two sets of
types.

## Decision

### The namespace is required

`[package].namespace` is required. A manifest whose `[package]` has none is an
error, as one without `name` or `version` is. A workspace member inherits the
namespace of `[workspace.package]`, as before.

Every package therefore has a coordinate, `namespace:name`, whether or not it is
published. What follows from that:

- A coordinate does not mean published. `publish = false` is the only way a
  package stays out of a registry, and publishing no longer checks the
  namespace.
- `wado init` takes `--namespace` as well as `--name`, and writes both. In a
  workspace member it writes neither the namespace nor the version that
  `[workspace.package]` sets, and refuses a `--namespace` there.
- An exported file and a Kiln input from another package are always named
  `namespace:name@version/path`.
- A `lib:` alias renames a dependency, shortens its name, or tells two major
  versions apart. It is no longer the only way to depend on a package, since
  every package has a coordinate a key can be.

### A coordinate key names its package

A `path` or `git` source under a coordinate key must hold the package whose
`[package]` declares that coordinate. A key whose source declares another one
is unresolved, and a `use` of it reports both coordinates.

A single-file `path` source has no manifest, so it declares no coordinate. Only
a `lib:` key names it.

### One coordinate is one package

Two directories declaring one coordinate, with versions in one compatibility
range, are one package claimed twice. The first one the build reaches holds the
package, the entry's own package first. A `use` that reaches the second is an
error naming both, at any depth in the dependency graph.

Versions in two compatibility ranges stay two packages, which is how two `lib:`
aliases depend on two major versions of one coordinate
([Packages](./spec-packages.md#version-requirements)).

### This repository names its packages by coordinate

A `lib:` key that only shortened a sibling's coordinate (`"lib:gale"` for
`wado-lang:gale`) becomes the coordinate itself, in every manifest and `use`.
`lib:` stays where a test or a document is about the alias.

## Roadmap

1. [x] `wado-manifest` refuses a `[package]` with no `namespace`, and
   `wado init` requires `--namespace` unless the workspace sets one.
2. [x] Every manifest in this repository declares a namespace or inherits one.
3. [x] A coordinate key is checked against the `[package]` its source holds,
   and a single file under one is refused.
4. [x] A second directory claiming a package already held is refused at the
   `use` that reaches it.
5. [x] This repository's manifests and `use`s name sibling packages by
   coordinate.
