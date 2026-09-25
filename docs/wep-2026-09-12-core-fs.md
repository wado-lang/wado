# WEP: Whole-File I/O (`core:fs`)

## Context

Reading one file from a Wado tool is 26 lines of `wasi:filesystem`, and writing
one is 28 ([#2036](https://github.com/wado-lang/wado/issues/2036)). The counts
are not the whole cost. Measured over the repository, excluding the stdlib and
the compiler's fixtures:

|                                                                                      | count                |
| ------------------------------------------------------------------------------------ | -------------------- |
| `Preopens::get_directories()` followed by `dirs[0].0`                                | 24 sites in 23 files |
| local `read_file` / `read_file_bytes` / `write_file` / `read_file_to_string` helpers | 21 in 16 files       |
| `read_via_stream` sites that bind the completion future as `_done`                   | 22 of 27             |

One of the 24 root lookups is inside a code generator (`highlight_gen.wado`),
which writes the boilerplate into every CLI it emits.

The last row is a bug rather than verbosity. `read_via_stream` returns a stream
and a future, and the future is where a mid-read I/O error is reported. 22 of
the 27 call sites drop it, so a failed read there is a short file rather than an
error. The issue reports this for `--paths-from`, where one package fixed it and
the other could not share the fix.

So the module is not sugar. It is the one place that knows which of a
transfer's handles owes a `drop()`, and that its completion future has to be
read.

## Decision

`core:fs` is whole-file I/O against the preopened directory tree, and nothing
else.

```wado
use fs from "core:fs";

pub use { Preopens, Descriptor, ErrorCode } from "wasi:filesystem";
pub use { Instant } from "wasi:clocks";

pub fn root() -> Result<Descriptor, FsError> with Preopens;

pub fn read<S: AsStrSlice>(path: S) -> Result<ByteList, FsError> with Preopens;
pub fn read_to_string<S: AsStrSlice>(path: S) -> Result<String, FsError> with Preopens;
pub fn write<T: AsByteSlice, S: AsStrSlice>(path: S, data: &T) -> Result<(), FsError> with Preopens;
pub fn remove_file<S: AsStrSlice>(path: S) -> Result<(), FsError> with Preopens;

pub fn write_in_place<T: AsByteSlice, S: AsStrSlice>(path: S, data: &T) -> Result<(), FsError> with Preopens;
pub fn rename<F: AsStrSlice, T: AsStrSlice>(from: F, to: T) -> Result<(), FsError> with Preopens;

pub fn metadata<S: AsStrSlice>(path: S) -> Result<Metadata, FsError> with Preopens;
pub fn exists<S: AsStrSlice>(path: S) -> bool with Preopens;
pub fn try_exists<S: AsStrSlice>(path: S) -> Result<bool, FsError> with Preopens;

pub fn read_dir<S: AsStrSlice>(path: S) -> Result<List<DirEntry>, FsError> with Preopens;
pub fn walk_dir<S: AsStrSlice>(path: S, descend: fn(&WalkEntry) -> bool = …) -> Result<List<WalkEntry>, FsError> with Preopens;
pub fn create_dir<S: AsStrSlice>(path: S) -> Result<(), FsError> with Preopens;
pub fn create_dir_all<S: AsStrSlice>(path: S) -> Result<(), FsError> with Preopens;
pub fn remove_dir<S: AsStrSlice>(path: S) -> Result<(), FsError> with Preopens;
pub fn remove_dir_all<S: AsStrSlice>(path: S) -> Result<(), FsError> with Preopens;

pub fn join<A: AsStrSlice, B: AsStrSlice>(base: A, name: B) -> String;
pub fn parent<S: AsStrSlice>(path: S) -> Option<StrSlice>;
pub fn file_name<S: AsStrSlice>(path: S) -> Option<StrSlice>;
pub fn file_stem<S: AsStrSlice>(path: S) -> Option<StrSlice>;
pub fn extension<S: AsStrSlice>(path: S) -> Option<StrSlice>;
pub fn normalize<S: AsStrSlice>(path: S) -> Result<String, FsError>;
```

A path is whatever views as text — a `String`, a reference to one, or a
`StrSlice` — so a caller splits a path and passes the halves back without
copying either.

### Paths, not descriptors

Every one of the 24 sites resolves against the first preopened directory, so
that is the default and the descriptor leaves the signatures. The host resolves
a path from that descriptor, and a path may name several components;
`scripts/rust-inline-paths.json` is a call the repository already makes. A
recursive walk therefore needs no descriptor either: `read_dir("a/b")` replaces
`open_dir` followed by `read_directory`.

The empty path and `"."` name the preopen itself, so a walk has a root case. An
absolute path resolves against no preopen and fails. The host answers that for
every operation but `create_dir_all`, which splits the path rather than handing
it over; it runs the path through `normalize` first, which makes the same check
and settles `.` and `..` before any directory is made.

`create_dir_all` makes one more check the host cannot: `Exist` reports whatever
occupies a component, so it reads the entry's `metadata` and accepts only a
directory. A regular file there is `NotDirectory`, not a directory that was
already made.
`create_dir` makes one directory and needs no such check, since it passes the
host's answer straight back, as Rust's `create_dir` does.

A second preopen is ignored. Nothing in the repository grants one. The
alternative, resolving a path against the longest matching preopen prefix as
wasi-libc does, is machinery with no caller. `root()` is the escape hatch for
anything this module does not cover, and returns the same descriptor.

### The error carries the path

Every call site prints the path with the error (`error: ${path}: cannot open:
${e:?}`), so the path belongs in the type rather than in 21 copies of the
message:

```wado
pub struct FsError { pub path: String, pub kind: FsErrorKind }

pub variant FsErrorKind {
    NoPreopen,
    NotFound,
    NotUtf8(Utf8Error),
    Io(ErrorCode),
}
```

A kind earns its name by a branch that exists: `NoPreopen` is reported at 7
sites, `NotFound` is what a tool distinguishes from a real fault, and `NotUtf8`
has no `ErrorCode` at all because it is not an I/O failure. Everything else
stays `Io(ErrorCode)`, which loses nothing: the code is the one WASI gave.
`AlreadyExists` is not a kind either. `create_dir_all` settles `Exist` itself,
either as the directory it asked for or as `NotDirectory`, and `create_dir`
hands the code back as `Io(Exist)`, which a caller matches like any other.

`impl Display for FsError` renders `path: message`, so a caller writes
`eprintln(`error: ${e}`)` and the message names the file.

### Effects stay `Preopens`

`core:fs` declares `with Preopens` and introduces no effect of its own. An
`interface FileSystem` that a test could install a handler for is not possible
today: an operation may not carry a `with` clause (`docs/spec-effects.md`, "Beyond a
name, parameters and a return type, an operation declares nothing else"), so its
default implementation could never perform `Preopens`. When that changes, the
functions here become the default implementations of such an interface without
changing their signatures.

### Whole files only

`read` buffers the whole file, and `write` puts one in place of whatever the
path held, as the section below describes. `read` also checks that the path
names a regular file, because `read_via_stream` traps on anything else, and a
trap aborts the program instead of returning the `Result` the signature
promises. Every other operation states what it expects in its open flags, so
the host makes that check.

Streaming stays on `wasi:filesystem`. `example/cat.wado` connects a file's read
stream straight to stdout and never holds the file in memory. This module would
break that shape rather than shorten it, so the example keeps its raw WASI code
on purpose.

`write` takes any `AsByteSlice` (`String`, `ByteList`, `ByteSlice`,
`ByteArray`), so text and bytes are one function. It writes through
`write_raw_all`, which hands the CM lowering a view instead of copying the
buffer first. A writer that calls `content.bytes().collect()` pays for that
copy.

Deliberately absent: append, symlink, file times, permissions, random access,
and reading a file that does not fit in memory. Each is `wasi:filesystem`
through `root()`. They are out because nothing calls them, here or in a module
written against this one. A caller that needs one is a reason to move the line,
as [#2061](https://github.com/wado-lang/wado/issues/2061) was for `remove_dir`.

### Removing a directory

`create_dir` and `create_dir_all` build a tree, so `remove_dir` and
`remove_dir_all` take one down. Without them a walk that clears an output
directory cannot finish. `read_dir` names the directories under it, and no call
removes them, so the last step drops back to `root()`.

The names and the split are Rust's. `remove_dir` hands the host's answer back,
so a directory that still holds an entry is `Io(NotEmpty)`. `remove_dir_all`
walks `read_dir` itself and removes an entry that is not a directory with
`remove_file`. A symlink is `Other` to that walk, so it is unlinked rather than
followed. Every other operation says the same with `PathFlags::none()`.

Two departures from Rust, both from this module's own shape:

- A path that names nothing is not a failure for `remove_dir_all`, as a
  `create_dir_all` finding the directory already there is not. What the call
  promises is that the tree is gone. Without that, a caller that clears its
  output before writing it matches `NotFound` on every first run. `remove_dir`
  keeps the strict reading, so a caller that wants to know still has one.
- A path that resolves to the preopen is `Io(NotPermitted)` for both. The
  preopen is the root every path resolves against, and a `wado run --dir` grant
  is not a reason to let a typo empty the working directory. A caller that means
  it walks `read_dir("")` and removes each entry.

  The guard resolves the path rather than reading it. `""` and `"."` are the
  obvious spellings. `"./"`, `"x/.."` and `"./x/../."` name it too. A guard that
  compared text would pass all three, and `remove_dir_all` starts its walk
  before the host ever sees the path, so it would empty the working directory
  and only then fail on the final `remove_dir`. `normalize` is the module's own
  resolver and answers `""` for every such spelling, so running the path through
  it is the whole check. It also refuses an absolute path and one that climbs
  out of the preopen, which a removal should refuse anyway.

One function with a `recursive: bool = false` parameter would collapse the pair,
and a default is trailing so it would fit. Wado has no named arguments
([WEP: Default Arguments](./wep-2026-04-11-default-arguments.md)), so the call
site reads `remove_dir(path, true)`. The flag that decides whether a tree
survives would be spelled as a bare `true`, which is why the pair stays.

### A write replaces the file, it does not truncate it

`write` puts its bytes in a new file beside the target and renames that over
it. A truncating write is only correct when nothing reads the file and nothing
interrupts the writer, and the repository's own writers are the other case:
they rewrite a `.md` the user is editing, a committed grammar, and a checked-in
baseline. A process that dies mid-write takes the original with it.

The temporary file is a sibling of the target, not a file in a temporary
directory. Rename is atomic within one filesystem and fails across two, the
host refuses a rename between two grants whose permissions differ, and WASI has
no temporary directory to reach for in the first place: the preopen tree is
all there is. The same reason puts Rust's `NamedTempFile::new_in` and Go's
`os.CreateTemp(filepath.Dir(dst))` next to their targets.

The name is `<target>.wado-tmp`, then `.1`, `.2` and on, counted from zero on
each write. The file is created with `Create | Exclusive`, so the host decides
uniqueness atomically and a second writer that loses the race takes the next
name.

A name at the host's length limit cannot take that suffix, and a target whose
own name fits is a file `write` has to be able to write. So `NameTooLong` on
the temporary file drops the target's name and keeps its directory: the next
candidate is `<dir>/.wado-tmp`, which is nine bytes whatever the target is
called. That happens once per write, and `Exclusive` settles the rest. A random name would settle the
same question, and cost `Random` in the signature of every function that writes
a file and `wasi:random` in its component's imports. Elsewhere a random name
defends a shared `/tmp` against a symlink another user planted. This tree has
no shared `/tmp`.

A failed write unlinks its temporary file, best effort: the write already
failed, so a failure to clean up is not a second error to report. A process
that dies between the create and the rename leaves one behind, which every
implementation of this pattern leaves behind.

No error names the temporary file. `write` has one exit that reports a failure,
and it puts the path the caller passed on whatever comes back. Without that, a
rename onto an occupied path reports `build/out.json.wado-tmp`, a file the
caller never wrote.

A rename replaces a symlink rather than following it. That is the one thing
this shape does that a truncating write could not: it detaches a link where
`write_in_place` refused with `Loop`. A tracked link replaced by a file is a
change the next `git status` reports. So
`write` refuses a target that is not a regular file. Every read here already
refuses one, and a write is not the place to start following links. What that
refusal promises is in "Time of check, time of use" below.

`write_in_place` keeps the truncating write for the callers that want it: a
file too large to exist twice, and a directory that should not gain a second
entry. The name says which one it is, so the safe spelling is also the short
one.

Atomicity is not durability, and `write` promises only the first. A reader sees
the old file or the new one and never a half-written one, but nothing here
calls `sync_data` on the file or `sync` on the directory, so power loss can
still take a write the host reported as complete. The doc comment says so.
A caller that needs otherwise syncs the descriptor `root()` hands over, which
is the escape hatch this module keeps for exactly this.

### Time of check, time of use

A path is a name, not a handle. Every call that reaches the filesystem resolves
it again, so what such a call reports is true of the moment it ran and not of
the moment the caller reads it. Asking first and acting after is two
resolutions with a window between them. The path functions are outside all of
this: they declare no effect and open nothing, so they have nothing to race.

Two calls here have such a window inside them. `write` reads the path to refuse
a target that is not a regular file, then renames over it. `create_dir_all`
reads a component the host answered `Exist` for, to find out whether it is a
directory. In each, a writer that changes the path inside the window gets the
action rather than the refusal.

Every other call is one the host settles by itself: one `open_at`, one
`rename_at`, one `unlink_file_at`. `read` refuses a directory by asking the
descriptor it opened, not by resolving the path a second time. `write` creates
its temporary file with `Create | Exclusive`, which is the host deciding a name
rather than this module checking one.

`remove_dir_all` and `walk_dir` are a third shape. Each acts on a listing, so a
tree that changes mid-walk answers partly from before and partly from after. A
`remove_dir_all` whose tree grows under it leaves the root `Io(NotEmpty)`.

None of these windows reaches outside the preopen. Every path opens with
`PathFlags::none()`, and a directory entry that is a symlink is `Other` rather
than `Directory`. So the swap that turns `rm -rf` into a way out of a sandbox
gets `Io(Loop)` or unlinks the link instead. A test covers that, because it is
the one property here a wrong answer would make dangerous.

Neither window is this module's to close. `wasi:filesystem` has no rename that
validates its destination, and none of its creates reports what it found. So
each doc comment says where its window is, and a caller that cannot tolerate
one owns the directory it writes in.

### `exists` answers `false`, `try_exists` answers why

`exists` folds everything that stopped the look into `false`. `try_exists`
reports it, and answers `false` only for "nothing is there". Rust splits the
pair the same way and gives the short name to the folding one, so this does
too.

A caller reaching for either is usually about to act on the answer, and the act
is the better question. Both doc comments say so.

### The `wasi:*` names in these signatures are re-exported

A caller of this module writes `Preopens` in every `with` clause and reads
`ErrorCode` out of an `Io` error. Importing `wasi:filesystem` for those two
names is importing the module this one exists to replace. So `core:fs`
re-exports what its own signatures carry: `Preopens`, `Descriptor` and
`ErrorCode` from `wasi:filesystem`, and `Instant` from `wasi:clocks`.

A file that calls `wasi:filesystem` itself still imports it. `example/cat.wado`
streams and `package-gale/src/main.wado` searches every grant, and this module
does neither. The re-export is for the caller that only needed the name.

### A walk is a list, because an iterator may not perform I/O

`walk_dir` lists a whole tree before it returns. The shape that suggests itself
is an iterator, and it is not available: every standard library trait declares
`with ()`, `Iterator` included, so a `next` that reads a directory is a design
error rather than a slow path. A walk too large to hold is `read_dir` and a
recursion of the caller's own, which is what `example/tree.wado` keeps.

The cost that shape would have saved is pruning, so `walk_dir` takes the
predicate instead: `descend` is asked about a directory before the walk enters
it, and the directory is listed either way. Without it the callers that skip
`.git` and `node_modules` would pay to list them.

An entry carries the path that reaches it rather than its name alone. A walk's
answer is read, removed or opened, and rebuilding the path from a name is the
step a caller would get wrong.

`descend` is handed that same path, so a predicate meaning a name takes
`file_name` of it. Comparing the path against a bare name matches only when the
walk started at the root the name sits in, which is why the pruning callers here
go through `file_name`.

### Path operations live here, not in a `core:path`

Joining, taking a parent or an extension, and normalizing a path are `core:fs`
functions. Every other language splits them off. The three reasons it has for
that are absent here.

Two of them are about what a path is. A separate path module carries an owned
path type (`PathBuf`, `OsString`) and the differences between one operating
system's path syntax and another's. A WASI path is UTF-8, `/`-separated and
relative to a preopen, so there is no syntax to abstract over, and a Wado
newtype carries no invariant its base lacks
([WEP: Newtype Semantics](./wep-2026-01-29-newtype-semantics.md)), so a
`type Path = String` would buy nothing that `AsStrSlice` does not already give.
Nothing is left for the module to own.

The third is about what a path means, and it points the other way. The rules
these functions apply are this module's: a leading `/` resolves against no
preopen, `""` and `"."` name the preopen itself, and a `..` that climbs out of
it is the sandbox boundary rather than a string. `create_dir_all` splits a path
and rejects a leading `/` to make those calls, and `remove_dir_all` resolves one
to find out whether it was handed the preopen. A module that did not know these
rules would have to restate them, and one that was free of them could not answer
`normalize` at all.

So the path functions are where these rules are applied, once. `file_name`
answers `None` for a component that is `.` or `..`, since neither names an
entry, and a caller matching a name against a list never has to recognize a
traversal. `extension` keeps Rust's answers, `Some("")` for `a/b.` among them.

The usual reason to split is to keep a caller that only joins strings away from
the filesystem. Wado already does that with effects: a path function declares no
`with` clause, so its signature says it performs no I/O, and calling one adds no
import to the component. `wasi:filesystem` enters the generated world only when
a call that performs `Preopens` is reached. The module boundary would repeat
what the effect already states.

`core:url` also joins and normalizes paths, and shares none of this. Its rules
come from the WHATWG URL standard, which resolves `..`, percent-encoding and
schemes its own way. Two standards that look alike are the reason to keep two
implementations, not to merge them.

If a caller with no filesystem at all appears, the pure half can move out and
`core:fs` can re-export it with `pub use`, so this is not a one-way door.

## Roadmap

Nothing is pending. The module answers what the Decision describes, and the
callers in this repository read, write and walk through it. What it still
cannot reach is unowned and sits below.

## Known gaps

- A caller that opens its own files still answers "is it there?" by discarding
  an error: `package-gale/src/main.wado` opens each grant in turn and
  `extract_antlr4_descriptors.wado` reads a file to find out whether it exists.
  What blocks them is the second preopen and the shadow copy below, not the
  question, which `try_exists` answers for every path this module can reach.
- `write` and `create_dir_all` each read a path before acting on it, so a
  writer that changes the path inside that window gets the action rather than
  the refusal. `wasi:filesystem` has neither a rename that validates its
  destination nor a create that reports what it found.
- A temporary file outlives a process that dies between creating it and
  renaming it, so a directory can collect `<name>.wado-tmp*` entries that no
  writer owns. What makes one stale is undecided, and while another process may
  be mid-write on the same name no answer is safe.
- `package-gale/scripts/extract_antlr4_descriptors.wado` keeps its own
  descriptor plumbing, a shadow copy of some seven functions here. Its helpers
  each take a subdirectory `Descriptor`, which this module cannot express
  because every path resolves against the first preopen, and it opens
  directories with `MutateDirectory`, which this module never requests. It also
  cannot run here at all without the `vendor/antlr4` submodule, so converting it
  is a refactor no test in this repository would check.
- A second preopen is unreachable: a path resolves against the first one, and
  what an ambiguous path means is undecided. `package-gale/src/main.wado` keeps
  its own opener for that reason: it searches every grant and names the ones it
  searched, which is what makes `wado run --dir` legible there.
- No handler can stand in for the filesystem, so a test of a caller still needs
  a real directory. An operation cannot declare an effect (`docs/spec-effects.md`,
  "Beyond a name, parameters and a return type, an operation declares nothing
  else"), which is what an `interface FileSystem` would rest on.
- A symlink is not followed: every path opens with `PathFlags::none()`, so
  reading one fails with `Loop` and writing one is refused. `metadata` reports
  the link itself as `Other`, so `exists` and `try_exists` both answer `true`
  for a path nothing here can read, a dangling link included. What a link
  pointing out of the preopen means is undecided.
- An unnamed cause renders through `Inspect`, so `Io(ErrorCode::Access)` reads
  as `path: ErrorCode::Access` rather than as prose. Prose would need a message
  for each of the 40 `ErrorCode`s, most of which no caller branches on.
