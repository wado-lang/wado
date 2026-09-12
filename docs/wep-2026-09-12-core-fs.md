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
the other could not share the fix. Only the five sites that happened to get it
right are fixed.

So the module is not sugar. It is the one place that can know which of the four
values (`file`, `rx`, `tx`, `sub`) owes a `drop()`, and that a completion future
has to be read.

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
pub fn create_dir_all(path: &String) -> Result<(), FsError> with Preopens;
```

### Paths, not descriptors

Every one of the 24 sites resolves against the first preopened directory, so
that is the default and the descriptor leaves the signatures. A path is
resolved by the host from that descriptor, which already accepts multiple
components (`scripts/rust-inline-paths.json` is an existing call), so a
recursive directory walk needs no descriptor either: `read_dir("a/b")` is what
`open_dir` + `read_directory` was.

The empty path and `"."` name the preopen itself, so a walk has a root case. An
absolute path resolves against no preopen and fails. The host answers that for
every operation but `create_dir_all`, which splits the path rather than handing
it over, so it makes the same check itself.

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
`create_dir_all` swallows `Exist` itself, which is why `AlreadyExists` is not a
kind.

`impl Display for FsError` renders `path: message`, so a caller writes
`eprintln(`error: ${e}`)` and gets what it used to spell out.

### Effects stay `Preopens`

`core:fs` declares `with Preopens` and introduces no effect of its own. An
`interface FileSystem` that a test could install a handler for is not possible
today: an operation may not carry a `with` clause (`docs/spec.md`, "Beyond a
name, parameters and a return type, an operation declares nothing else"), so its
default implementation could never perform `Preopens`. When that changes, the
functions here become the default implementations of such an interface without
changing their signatures.

### Whole files only

`read` buffers, `write` creates-truncates-and-closes. `read` also checks that
the path names a regular file. `read_via_stream` traps on anything else, which
aborts the program instead of returning the `Result` the signature promises.
Every other operation states what it expects in its open flags, so the host
makes that check.

Streaming stays on `wasi:filesystem`. `example/cat.wado` connects a file's read stream straight to
stdout and never holds the file in memory. This module would break that shape
rather than shorten it, so the example keeps its raw WASI code on purpose, as
the worked case of the boundary.

`write` takes any `AsByteSlice` (`String`, `ByteList`, `ByteSlice`,
`ByteArray`), so text and bytes are one function. It writes through
`write_raw_all`, which hands the CM lowering a view instead of copying the
buffer first. That copy is the `content.bytes().collect()` every current writer
pays for.

Deliberately absent: append, rename, symlink, `remove_dir`, metadata / `stat`,
file times, permissions, random access, and reading a file that does not fit in
memory. Each is `wasi:filesystem` through `root()`. They are out because no
caller in the repository needs them, not because they would not fit.

## Roadmap

1. `lib/core/fs.wado`, registered in `src/stdlib.rs`, tested by
   `lib/core/fs_test.wado` beside it: the round trips, `NotFound`, `NotUtf8`,
   the nested `create_dir_all` / `read_dir` / `remove_file` path, and the empty
   path. `wado test` preopens the working directory, so each test owns a
   directory under `target/` and reads only what it wrote there. Done when that
   file passes at every optimization level.
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

## Known gaps

- `package-gale/scripts/extract_antlr4_descriptors.wado` keeps its own
  descriptor plumbing. It threads nested `Descriptor`s through some twenty
  sites, and it cannot run here at all without the `vendor/antlr4` submodule, so
  converting it is a refactor no test in this repository would check. Closing it
  means porting those sites to paths and re-running the vendor extract.
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
- An unnamed cause renders through `Inspect`, so `Io(ErrorCode::Access)` reads
  as `path: ErrorCode::Access` rather than as prose. Closing it means a message
  per `ErrorCode`, which is 40 strings for the codes no caller branches on.
