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

pub fn root() -> Result<Descriptor, FsError> with Preopens;

pub fn read(path: &String) -> Result<ByteList, FsError> with Preopens;
pub fn read_to_string(path: &String) -> Result<String, FsError> with Preopens;
pub fn write<T: AsByteSlice>(path: &String, data: &T) -> Result<(), FsError> with Preopens;
pub fn remove_file(path: &String) -> Result<(), FsError> with Preopens;

pub fn read_dir(path: &String) -> Result<List<DirEntry>, FsError> with Preopens;
pub fn create_dir(path: &String) -> Result<(), FsError> with Preopens;
pub fn create_dir_all(path: &String) -> Result<(), FsError> with Preopens;
pub fn remove_dir(path: &String) -> Result<(), FsError> with Preopens;
pub fn remove_dir_all(path: &String) -> Result<(), FsError> with Preopens;
```

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
it over, so it makes the same check itself.

`create_dir_all` makes one more check the host cannot: `Exist` reports whatever
occupies a component, so it stats the entry and accepts only a directory. A
regular file there is `NotDirectory`, not a directory that was already made.
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
    NotUtf8,
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
today: an operation may not carry a `with` clause (`docs/spec.md`, "Beyond a
name, parameters and a return type, an operation declares nothing else"), so its
default implementation could never perform `Preopens`. When that changes, the
functions here become the default implementations of such an interface without
changing their signatures.

### Whole files only

`read` buffers the whole file, and `write` creates or truncates one and closes
it. `read` also checks that the path names a regular file, because
`read_via_stream` traps on anything else, and a trap aborts the program instead
of returning the `Result` the signature promises. Every other operation states
what it expects in its open flags, so the host makes that check.

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
The roadmap carries the ones a caller has already reached for.

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
- `""` and `"."` are `Io(NotPermitted)` for both. They name the preopen, which
  is the root every path resolves against, and a `wado run --dir` grant is not a
  reason to let a typo empty the working directory. A caller that means it walks
  `read_dir("")` and removes each entry.

One function with a `recursive: bool = false` parameter would collapse the pair,
and a default is trailing so it would fit. Wado has no named arguments
([WEP: Default Arguments](./wep-2026-04-11-default-arguments.md)), so the call
site reads `remove_dir(path, true)`. The flag that decides whether a tree
survives would be spelled as a bare `true`, which is why the pair stays.

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
it is the sandbox boundary rather than a string. `create_dir_all` already
splits a path and rejects a leading `/` to make those calls. A module that did
not know them would have to restate them, and one that was free of them could
not answer `normalize` at all.

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

1. `lib/core/fs.wado`, registered in `src/stdlib.rs`, tested by
   `lib/core/fs_test.wado` beside it: the round trips, `NotFound`, `NotUtf8`,
   the nested `create_dir_all` / `read_dir` / `remove_file` path, the removal of
   a tree, and the empty path. `wado test` preopens the working directory, so
   each test owns a directory under `target/` and reads only what it wrote
   there. Done when that file passes at every optimization level.
2. The call sites the issue names: `package-gale/tools/rust_corpus.wado`,
   `rust_corpus_check.wado`, `rust_inline_paths.wado`, and
   `package-gale-highlight-wado/tools/{corpus,corpus_check,highlight_dump}.wado`.
   Done when `scripts/check-rust-paths.sh --check` and
   `scripts/check-highlight.sh` pass, which read the whole corpus through
   `core:fs`.
3. The rest of the whole-file readers and writers: `package-marl/src/main.wado`,
   `package-gale/src/highlight/facade.wado`, the ten `benchmark/*` copies,
   `example/tree.wado`, the four `package-gale/scripts/*.wado` that only read or
   write, and `package-gale/src/highlight_gen.wado`, which emits the boilerplate
   rather than running it. Done when each package's tests pass,
   `expand_action_templates_in_place.wado` reports its corpus unchanged, and the
   regenerated highlighters carry the shorter `run`.
4. Path operations: `join`, `parent`, `file_name`, `file_stem`, `extension`,
   and `normalize`. Everything but `normalize` is pure string work over
   `AsStrSlice`, and `normalize` applies the preopen rules above, so it answers
   a `Result`. The views come back as `StrSlice`, so taking a parent or an
   extension copies nothing. This goes first because the call sites of 5 and 7
   name a file and test its extension. Done when the four helpers that do this
   by hand are gone — `join_path` in `package-marl/src/main.wado` and
   `example/tree.wado`, which are byte-identical, `parent_of` in
   `package-gale/src/highlight/facade.wado`, and `grant_hint` in
   `package-gale/src/main.wado` — the two `substr_bytes(0, len - 4)` extension
   strips in `package-gale/scripts/extract_antlr4_descriptors.wado` read
   `file_stem`, and `create_dir_all` and `remove_dir_all` call the public
   functions instead of splitting and joining themselves.
5. `metadata`: the type, the size, and the modification time of what a path
   names, plus the `exists` that reads from it. Thirteen places answer "is it
   there?" by discarding the error of a call made for another purpose, and
   `package-marl/src/main.wado` decides file-or-directory from the `.md` suffix
   because no call answers it. `stat_at` is already here, privately, inside
   `ensure_dir`. Done when those sites ask instead, and `ensure_dir` asks
   through the public function.
6. `rename`, and with it the write that does not destroy what it replaces.
   Nine writes truncate a file the user already has, among them the markdown
   formatter rewriting a `.md` in place and the two scripts that rewrite
   committed `.g4` and baseline files, so a process that dies mid-write takes
   the original with it. `extract_antlr4_descriptors.wado` already builds a
   two-file rollback out of `remove_file` for the same reason. Done when
   `rename` exists with tests, and the in-place rewrites that can reach it no
   longer truncate their target.
7. `walk_dir`: every entry under a path, depth-first, as an iterator of the
   path and its type. `read_dir` lists one level, so four places write the
   recursion themselves, one of them chunking the raw directory stream 64
   entries at a time. It needs no WASI surface this module does not already
   use. Done when `package-marl/src/main.wado` and `example/tree.wado` walk
   through it and their own recursion is gone.

## Known gaps

- Roadmap item 6 does not say what shape the safe write takes: whether `write`
  itself becomes write-elsewhere-then-rename, or a second function carries it
  and `write` keeps truncating. Making every `write` atomic costs a second path
  in the same directory and changes what a reader sees mid-write, which is a
  choice for whoever takes the item.
- `package-gale/scripts/extract_antlr4_descriptors.wado` keeps its own
  descriptor plumbing, a shadow copy of some seven functions here. Its helpers
  each take a subdirectory `Descriptor`, which this module cannot express
  because every path resolves against the first preopen, and it opens
  directories with `MutateDirectory`, which this module never requests. It also
  cannot run here at all without the `vendor/antlr4` submodule, so converting it
  is a refactor no test in this repository would check. Closing it means writing
  those sites as preopen-relative paths, or giving this module a handle for a
  directory below the preopen, and then re-running the vendor extract.
- A second preopen is unreachable. Closing it means resolving a path against the
  preopen whose name is its longest matching prefix, and deciding what an
  ambiguous path does. `package-gale/src/main.wado` keeps its own opener for
  that reason: it searches every grant and names the ones it searched, which is
  what makes `wado run --dir` legible there.
- No handler can stand in for the filesystem, so a test of a caller still needs
  a real directory. Closing it needs an operation to be able to declare an
  effect (`docs/spec.md`, "Beyond a name, parameters and a return type, an
  operation declares nothing else"), after which these functions become the
  defaults of an `interface FileSystem`.
- A symlink is not followed: every path opens with `PathFlags::none()`, so
  reading one fails with `Loop`. Closing it means passing `SymlinkFollow` and
  deciding what a link that points out of the preopen does.
- An unnamed cause renders through `Inspect`, so `Io(ErrorCode::Access)` reads
  as `path: ErrorCode::Access` rather than as prose. Closing it means a message
  per `ErrorCode`, which is 40 strings for the codes no caller branches on.
