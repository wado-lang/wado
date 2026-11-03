# Profile-Guided Optimization

A package records how often its code runs under its real workload, commits the
record as a profile, and a build of it compiles by the profile. A profile
changes only what a program costs, never what it computes.

## Recording a Profile

`wado test --pgo` runs only the [`#[workload]`](./spec-attributes.md#workload)
tests, and records how often each region of the modules they reach ran. A
region is as [Coverage](./spec-testing.md#coverage) defines it. It measures the
package's modules, its dependencies' and the `core:` modules.

A package records all of its workloads into one profile, `pgo/default.pgo`
under its root. A file in no package records into `pgo/<stem>.pgo` in its own
directory. A run that leaves out a test file of a package it reaches is an
error, since it would overwrite the profile with part of the workloads. So
`--filter`, `--test-name` and `--exclude` are errors with `--pgo`, as is a path
argument naming part of a package. So are `--coverage` and `--no-run`.

The profile is written only when the run passes with no pending `#[TODO]`, ran
a workload, and saw no source file change while it ran. Otherwise the run
fails, and an existing profile is kept. When it is written, the profile of a
package the run holds no workload of is removed: it would hold counts from
workloads that are gone.

## The Profile Format

A profile is text, one record per line:

```text
wado-pgo 1
module acme:app@0.1.0/src/lib.wado
fn 3f2a9c0d1e2b4a59 12 pick
0 5
1 4
```

A `module` line names a module the same way on any machine and from any entry
of one package:

- A file of a package whose `wado.toml` has a `[package]`, the recording
  package's own or a dependency's, is the package's `namespace:name@version`,
  then the file relative to its root: `acme:dep@0.1.0/src/lib.wado`.
- Any other file is relative to the recording package's root, or for a program
  in no package, to the program's directory.
- A `core:` module is its import path, and a remote module its URL.

Each `fn` line gives a function's hash, its line and its name. The hash covers
the function's source text, a CRLF read as LF, and its regions' kinds and
positions relative to it. The lines after it give how often each of the
function's regions ran, the regions numbered from zero within the function. A
region not listed never ran.

A count of zero and no profile are different things. Every function of a
module the profile lists has a `fn` line, one the workloads never ran
included, and each of its regions has a count, zero where no line gives one.
A function with no `fn` line has no profile at all, and neither does one whose
hash differs from its current source's: nothing measured it as it stands, so
no count applies to it.

## Applying a Profile

A build applies its entry's profile, the one `wado test --pgo` records the
entry's workloads into. `-f pgo` turns this on and `-f no-pgo` off. It is on
by default in every world, the test world included, so a test runs the code a
build ships. A build that finds no profile compiles without one, and under
`-f pgo` says so at `--log-level info`.

`wado test --pgo` applies no profile: it would read the file it rewrites, and
no count depends on what a profile marks.

### Rarity

A compile applying a profile judges each region of a function the workloads
entered, and whose hash matches, on two questions:

- Is it unlikely? A region is unlikely when it ran at most `100 - n` percent of
  the times the code around it ran, where `n` is
  `--optimize-predictable-branch-threshold`, 99 by default. A function's body
  is entered by a call, so it is never unlikely.
- Is it cold? A region is cold when it never ran. One that ran is not, however
  rarely: a call in it may reach code that ran often.

A function the workloads never entered is not judged: workloads are tests, so a
build's own entry, and whatever only it reaches, never runs under them. Nor is
one whose hash differs, and the compile says so at `--log-level info`. Each is
compiled as without a profile, with the markers its source states.

### Markers

The compile marks the start of each region by its answers. An unlikely and
cold region gets the marker
[`builtin::cold_path()`](./spec-control-flow.md#branch-hints) writes, which
hints its branch and has the optimizer treat its code as rarely run. An
unlikely region that is not cold gets the hint alone, and a cold region that is
not unlikely gets the treatment alone. A region under one that never ran is
left to that region's marker.

In a function it judges, the profile overrides the markers the source states:
an author's `cold_path()`, and the one the compiler puts on the failure path of
a `?`. Such a marker is dropped, and the region's own marks stand. Where the
profile's answers are not both yes, the compile names the marker, the count,
and what the region was found to be at `--log-level info`.

Rationale: [WEP: Profile-Guided Optimization](./wep-2026-10-25-profile-guided-optimization.md).
