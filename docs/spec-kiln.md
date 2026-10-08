# Kiln Generators

Kiln is Wado's compiler plugin mechanism for generated imports. A generator is a
Wado program compiled to a component. The compiler runs it at build time to turn
an input file into Wado source, and then compiles that source like any
hand-written module. A `use` names the input file, and its `with` clause names
the generator ([Import Attributes](./spec-modules.md#import-attributes-with)).

Which `use` goes through Kiln is stated in
[How an Import Is Read](./spec-modules.md#how-an-import-is-read).

<!-- {"source": "wado-cli/tests/fixtures/kiln_nested/src/main.wado"} -->

```wado
use { model } from "./model.spec"
    with {
        generator: { module: "./outer.wado" },
    };
```

Rationale: [WEP: Kiln](./wep-2026-04-12-kiln.md).

## Importing a Generated Module

### The `generator` Fields

The literal after `from` is the primary input. It is a `./` or `../` path
resolved against the declaring file, like a local module import. The
`generator` object holds the rest of the invocation.

| Field        | Required | Meaning                                                                                                                                                                                           |
| ------------ | -------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `module`     | yes      | The generator: a `./` / `../` path to its source, or a `<namespace>:<name>[@<version>]` coordinate or `lib:<nick>` alias resolved against `[build-dependencies]`. A bare name is an error.        |
| `version`    | no       | Exact version of a coordinate `module`, for a file with no `wado.toml`. An error beside a path `module`, or when the manifest declares the generator.                                             |
| `registry`   | no       | Registry of a coordinate `module` (`oci://<host>[/<prefix>]`), under the same conditions as `version`.                                                                                            |
| `options`    | no       | Record literal whose shape matches the generator's exported `pub struct Options`. See [Options](#options).                                                                                        |
| `inputs`     | no       | Supplementary input paths (`./` / `../`) the generator cannot discover from the primary alone, such as a sibling lexer grammar. A schema that refers to other files lists every one of them here. |
| `output_dir` | no       | A `./` / `../` directory, resolved against the declaring file, that receives the generated files. Default `build/kiln/<synthesized-id>/` under the package root.                                  |

Each field has one type, and any other key or type is an error at the use site.

Beside `generator`, the `with` clause may hold `type`, a string. The generator
receives it as `Request.type`, and decides what it means: it is how a generator
that reads more than one kind of file is told which one this is. Any other key
beside `generator` is an error at the use site. A `type` makes the invocation a
different one, so the same file under two types is generated twice.

A `[build-dependencies]` coordinate names a published generator. Here Gale, a
generator that builds a parser from an ANTLR4 grammar
([WEP: Gale](./wep-2026-03-02-gale.md)), comes from a registry. The use site
supplies every option, for the reason [Options](#options) gives:

<!-- {"source": "example/hello-packages/src/main.wado"} -->

```wado
use calc from "./Calc.g4"
    with {
        generator: {
            module: "wado-lang:gale",
            options: { highlight: false, trace: false },
        },
    };
```

A `lib:` alias names a generator the package lists under a nickname. This clause
also hands the generator a supplementary input:

<!-- {"source": "package-gale-highlight-wado/src/lib.wado"} -->

```wado
use { highlight as highlight_impl } from "../grammar/Wado.g4"
    with {
        generator: {
            module: "lib:gale",
            inputs: ["../grammar/Wado.highlights.scm"],
            options: {
                fragment_entries: ["statement"],
            },
        },
    };
```

Every file a generator sees is named literally at the use site. There is no
glob and no directory listing, so the whole input set is known before any
generator runs.

### Binding the Import

A generator emits exactly one entry module and zero or more supplementary
modules. The `use` binds against the entry module, under the ordinary
visibility rules. Supplementary modules are ordinary Wado files that the entry
reaches with ordinary `use` statements. The entry module does not need to exist
before the first compile, because the generator runs before the import is
resolved.

Clauses are collected from every module the program reaches, so a module deep
in the graph can import a generated module of its own.

Generated files are compiled exactly like hand-written source, so a generator
that emits invalid Wado fails with an ordinary error against the generated file
on disk. Only the diagnostics a generator reports itself point into its input
files.

### Import Errors

A `use` whose generator produced no module for that schema is
`KILN_NO_GENERATED_MODULE`. The compiler never falls back to parsing the schema
as Wado.

Two clauses that agree on `module`, `version`, `registry`, the primary input,
`inputs`, `options`, and `output_dir` are one invocation, wherever in the
program they appear. The generator runs once for them. Two clauses that share a
primary input but disagree on any of those are an error naming both.

## Options

A generator declares its options as `pub struct Options` in its entry module.
Gale declares two:

<!-- {"source": "package-gale/src/generator.wado"} -->

```wado
pub struct Options {
    /// Emit trace logging to stderr in the generated parser: rule
    /// enter/ok/fail frames plus multi-alt scan/pick/try decisions. Off by
    /// default; set `trace: true` to debug why a grammar rejects an input.
    pub trace: bool = false,
    /// Rule names a fragment is a sequence of, for tooling (highlight / LSP);
    /// e.g. `["statement"]`. Empty is byte-identical. See "Parsing a fragment"
    /// in the README.
    pub fragment_entries: List<String>,
}
```

Every use site's `options` is checked against the struct before the generator
runs, so a typo or a type mismatch is reported on the offending key.

- Omitting `options` means every field takes its default. A field with a
  default may be omitted on its own.
- A field of type `Option<T>`, `List<T>`, or `TreeMap<String, V>` may always
  be omitted. It is then `None`, the empty list, or the empty map.
- Any other field without a default must be supplied. An unknown key is an
  error.
- An `enum` option is written as its case name in a string.
- A `TreeMap<String, V>` option is written as an object whose keys are the
  author's own. The generator receives it sorted by key.
- A generator resolved as a prebuilt component carries no field defaults, so
  at its use sites every field is required unless its type is an `Option`, a
  `List`, or a `TreeMap`.

Loam's options hold a list of structs, a map, and a list of strings:

<!-- {"source": "package-loam/src/generator.wado"} -->

```wado
pub struct Options {
    /// Patterns naming the axes of the graph's inputs and initializers.
    pub layout: List<LayoutRule>,
    /// Static extents for the graph's symbolic dimensions, keyed by `dim_param`.
    pub dims: TreeMap<String, i32>,
    /// The graph outputs `forward` returns. Empty keeps every output.
    pub outputs: List<String>,
}
```

Each struct in `layout` is written as a record literal, and the `dims` map as an
object:

<!-- {"source": "package-loam/conformance/specialize_test.wado"} -->

```wado
use { Batch, Col, Row, Weights, forward } from "./specialize.onnxtext"
    with {
        type: "onnx",
        generator: {
            module: "lib:loam",
            options: {
                layout: [
                    {
                        pattern: "X",
                        axes: ["Batch", "Row", "Col"],
                    },
                ],
                dims: { N: 2 },
                outputs: ["Y"],
            },
        },
    };
```

A map reaches the generator in key order. Given
`options: { sizes: { small: 1, large: 3 } }`, this generator spells out
`large=3,small=1`:

<!-- {"source": "wado-cli/tests/integration/kiln_options.rs"} -->

```wado
pub struct Options {
    pub sizes: TreeMap<String, i32>,
}

export fn generate(req: Request<Options>) -> Result<Response, Error> {
    let mut spelled: List<String> = [];
    for let [k, v] of req.options.sizes.entries() {
        spelled.push(`${k}=${v}`);
    }
    return Result::Ok(Response {
        files: [OutputFile {
            path: "greeting.wado",
            content: `pub fn greeting() -> String { return "${spelled.join(",")}"; }`,
            is_entry: true,
        }],
    });
}
```

An `Options` field is one of `bool`, a fixed-width integer, `f32`, `f64`,
`String`, a payload-less `enum`, a non-recursive `struct` of such fields,
`Option<T>`, `List<T>`, or `TreeMap<String, V>`. A field default is a literal.
Any other field fails the generator's own compile, since no use site could
supply it.

## Manifest

A generator named by a coordinate or a `lib:` alias is declared in
`[build-dependencies]` of `wado.toml`. Those entries form a graph of their own,
used only at build time and apart from the program's `[dependencies]`:

```toml
[build-dependencies]
"wado-lang:gale" = { version = "^0.0.9" }
```

A generator named by a relative path needs no entry. The path names its entry
module directly.

A consumer whose generated code calls the generator's runtime library also lists
the package under `[dependencies]` ([Runtime Libraries](#runtime-libraries)).

## Authoring a Generator

### The Generator World

A generator targets the `core:kiln/generator` world. It imports one interface,
`KilnHost`, and nothing else:

<!-- {"source": "wado-compiler/lib/core/kiln/worlds.wado"} -->

```wado
#[cm("core:kiln/generator@0.1.0")]
pub world Generator {
    import KilnHost;
}
```

A generator package maps the world to its entry module in its `[world]` table
([Selecting a World](./spec-worlds.md#selecting-a-world)). Grog, a Protocol
Buffers compiler, declares only this world:

```toml
[world]
"core:kiln/generator" = "src/generator.wado"
```

The entry module exports `generate`. It takes one `Request<Options>` and returns
a `Response` or an `Error`:

<!-- {"fixture": "spec_modules_kiln_generator.wado"} -->

```wado
use { Request, Response, OutputFile, Error, read_text } from "core:kiln";

pub struct Options {
    namespace: String,
    emit_comments: bool = true,
}

export fn generate(req: Request<Options>) -> Result<Response, Error> {
    let Ok(schema) = read_text(req.primary.content) else {
        return Result::Err(Error::InvalidSchema("not UTF-8"));
    };
    let source = emit(&schema, &req.options);   // the generator's own work
    return Result::Ok(Response {
        files: [OutputFile { path: `${req.options.namespace}.wado`, content: source, is_entry: true }],
    });
}

test {
    let options = Options { namespace: "calc" };
    assert options.emit_comments && emit(&"x", &options) == "// calc\nx";
}
```

A generator with no configuration declares no `Options` and writes
`fn generate(req: Request)`. A `generate` that reports diagnostics declares
`with KilnHost`. Grog's does both:

<!-- {"source": "package-grog/src/generator.wado"} -->

```wado
export fn generate(req: Request) -> Result<Response, Error> with KilnHost {
    let path = req.primary.path;
    let source = match read_text(req.primary.content) {
        Ok(text) => text,
        Err(e) => {
            return Result::Err(Error::InvalidSchema(`grog: '${path}' is not UTF-8: ${e}`));
        },
    };
    let runtime = runtime_of(&req.module)?;
    let file = read(&source).map_err(|e| at(&path, EmitError::Invalid(e)))?;
    let generated = emit(&file, runtime).map_err(|e| at(&path, e))?;
    let files: List<OutputFile> = [
        OutputFile {
            path: `${stem(&path)}.wado`,
            content: generated,
            is_entry: true,
        },
    ];
    return Result::Ok(Response { files });
}
```

`Request` exists only on the Wado side. At the component boundary `generate`
takes the four fields as four parameters, and a generator with no `Options`
takes no `options` parameter. Each generator's world is therefore its own:

```text
export generate: func(
    primary: input-file,
    inputs: list<input-file>,
    module: string,
    options: <the generator's own options record>,
) -> result<response, error>;
```

### The Request

<!-- {"source": "wado-compiler/lib/core/kiln.wado"} -->

```wado
pub struct Request<T = NoOptions> {
    pub primary: InputFile,
    pub inputs: List<InputFile>,
    /// How the use site named this generator: its `module:` specifier as
    /// written, or for a path, that path from the project root.
    pub module: String,
    /// The `type` the use site wrote beside `generator`, which says how the
    /// generator reads the file. `None` where it wrote none.
    pub type: Option<String>,
    pub options: T,
}
```

- `primary` is the input after `from`. `inputs` are the `inputs` of the clause,
  in the order written.
- `module` is how the use site named the generator: the `module:` specifier as
  written, or for a relative path, that path from the project root.
- `type` is the `type` beside `generator`, or `None`.
- `options` is the use site's options, with defaults filled in.

An input file carries its path from the project root, normalized, and the
content as a byte stream. So `./sub/../sub/v.txt` written in `src/main.wado`
arrives as `src/sub/v.txt`:

```wit
record input-file {
    path: string,
    content: stream<u8>,
}
```

`read_all` and `read_text` read a whole file
([`core:kiln`](./stdlib-core-kiln.md)). Gale reads every input as text, and
reports one that is not UTF-8 as an invalid schema:

<!-- {"source": "package-gale/src/generator.wado"} -->

```wado
fn decode_input(path: &String, content: Stream<u8>) -> Result<String, Error> {
    return match read_text(content) {
        Ok(text) => Result::Ok(text),
        Err(e) => Result::Err(Error::InvalidSchema(`gale: '${path}' is not UTF-8: ${e}`)),
    };
}
```

A generator that needs only a prefix reads the stream itself and drops it early.
Loam reads a checkpoint only as far as its header ends:

<!-- {"source": "package-loam/src/generator.wado"} -->

```wado
fn read_header(content: Stream<u8>) -> ByteList {
    let mut prefix = ByteList::with_capacity(HEADER_CHUNK);
    loop {
        let chunk = content.read(HEADER_CHUNK);
        prefix.extend(&(chunk.items as ByteList));
        if chunk.result != CopyResult::Completed || holds_header(&prefix) {
            break;
        }
    }
    content.drop();
    return prefix;
}
```

### The Response

A successful run returns the generated files:

```wit
record output-file {
    path: string,
    content: string,
    is-entry: bool,
}

record response {
    files: list<output-file>,
}
```

- `path` is relative to the invocation's output directory.
- `content` is the Wado source.
- Exactly one file has `is-entry` set. It is the entry module the `use` binds
  against.

The generator chooses the file names. Gale names its entry after the grammar:

<!-- {"source": "package-gale/src/generator.wado"} -->

```wado
let entry_name = to_snake_case(&grammar.name);
let files: List<OutputFile> = [
    OutputFile {
        path: `${entry_name}.wado`,
        content: wado_source,
        is_entry: true,
    },
];
return Result::Ok(Response { files });
```

### Output Directory

Outputs are written to the invocation's output directory, each stamped with a
`#![generated(by = "...", sources = [...])]` header
([`#![generated]`](./spec-attributes.md#generated)). The header names the
generator and its inputs from the package root:

<!-- {"source": "package-gale/tests/generated/cst_calc/calc_ll.wado"} -->

```wado
#![generated(by = "gale", sources = ["tests/grammars/calc_ll.g4"])]
```

That output was written to a directory the use site chose:

<!-- {"source": "package-gale/tests/driver_cst_calc_test.wado"} -->

```wado
use calc from "./grammars/calc_ll.g4"
    with {
        generator: { module: "../src/generator.wado", output_dir: "./generated/cst_calc" },
    };
```

A file in the output directory that carries the header belongs to the
invocation, and a later run may overwrite or remove it. A file without it is
left alone.

### Generator Errors

A generator that cannot produce output returns an `Error`:

```wit
variant error {
    invalid-schema(string),
    unsupported(string),
    other(string),
}
```

`invalid-schema` says an input is wrong. `unsupported` says the generator does
not handle what it was asked to do. `other` covers the rest. Each carries a
message, and the build fails reporting it.

A generator that traps fails the build too.

An error carries no span. Grog names the file in every message instead:

<!-- {"source": "package-grog/src/generator.wado"} -->

```wado
fn at(path: &String, e: EmitError) -> Error {
    return match e {
        Invalid(m) => Error::InvalidSchema(`grog: '${path}':${m}`),
        Unsupported(m) => Error::Unsupported(`grog: '${path}':${m}`),
    };
}
```

### Diagnostics

A generator reports diagnostics through the `KilnHost` effect. Each one has a
level, a message, and an optional span:

```wit
emit-diagnostic: func(diagnostic: diagnostic);

record diagnostic {
    level: diagnostic-level,
    span: option<source-span>,
    message: string,
}

enum diagnostic-level { error, warning, info, hint }

record source-span {
    path: string,
    byte-start: u32,
    byte-end: u32,
}
```

- A diagnostic surfaces as an ordinary compile diagnostic at its level.
- A span names one of the files the generator received, by the path that file
  carried, and a byte range in it.
- Every diagnostic is reported, whether the run then succeeds or fails.

Gale relays the diagnostics of its own analysis:

<!-- {"source": "package-gale/src/generator.wado"} -->

```wado
for let d of &generated.diagnostics {
    if let Some(level) = diagnostic_level(&d.kind) {
        KilnHost::emit_diagnostic(KilnDiagnostic {
            level: kiln_level(level),
            span: null,
            message: `gale: ${d.owner.label()}: ${d.message}`,
        });
    }
}
```

Loam reports a successful check as a hint:

<!-- {"source": "package-loam/src/generator.wado"} -->

```wado
KilnHost::emit_diagnostic(KilnDiagnostic {
    level: DiagnosticLevel::Hint,
    span: null,
    message: `loam: ${graph.name}: ${graph.nodes.len()} operators checked`,
});
```

### The Probe

A generator may export `probe` beside `generate`. It reports how many leading
bytes of an input determine the output. The host calls it once per input file,
with that file's path and content, and with the options when the generator
declares an `Options` struct.

This probe says the first line decides everything:

<!-- {"source": "wado-cli/tests/integration/kiln_probe.rs"} -->

```wado
export fn probe(path: String, content: Stream<u8>) -> Result<u64, Error> {
    let mut n: u64 = 0;
    loop {
        let chunk = content.read(1);
        if chunk.items.len() > 0 {
            n += 1;
            if chunk.items[0] == b'\n' {
                break;
            }
        }
        if chunk.result != CopyResult::Completed {
            break;
        }
    }
    content.drop();
    return Result::Ok(n);
}
```

- `generate` sees each input cut to its extent. Reading further gets the end
  of the stream.
- The extent must be decided by the bytes inside it.
- An extent that reaches the end of the file counts as the whole file.
- A generator that exports no probe depends on every byte of every input.

## The Sandbox

A generator runs in a deterministic sandbox. It has no clocks, randomness,
network, environment, or filesystem. It is a function of what the invocation
hands it: the input files, the name it was invoked under, and the options.

- The host provides `KilnHost` and nothing else.
- A generator opens no file. Each input arrives as a stream, and the host
  decides what is on it.
- A generator writes no file. Every output is returned in the response.
- A generator that imports a `wasi:*` interface, directly or through a
  `core:*` module, is a compile error (`KILN_GENERATOR_FORBIDDEN_IMPORT`).

### Running Again

A generator runs again only when something it depends on changes:

- the generator's identity and source, including the generated modules its own
  source imports;
- the content of each input, up to its extent;
- the options;
- the name the use site invoked it under;
- the version of the `core:kiln/generator` world.

The compiler version is not among them. An invocation whose dependencies are
unchanged reuses its recorded outputs without running the generator.

| Situation                                            | Behaviour                                                                          |
| ---------------------------------------------------- | ---------------------------------------------------------------------------------- |
| Nothing recorded                                     | Run the generator. Write its outputs and the record.                               |
| Dependencies unchanged, outputs as recorded          | Skip the generator.                                                                |
| Dependencies unchanged, an output was edited         | Skip the generator, warn (`KILN_GENERATED_MODIFIED`), and compile the edited file. |
| Dependencies changed, an existing output now differs | Run the generator, warn (`KILN_GENERATED_REGENERATED`), and overwrite the file.    |

The record is a file beside the outputs. Losing it is never an error: the next
compile runs the generator again.

The default output directory is under `build/`, so a fresh checkout runs the
generator again. A tracked `output_dir` commits the outputs and their record
together, so a fresh checkout reuses them.

`wado check` runs generators exactly as a build does. It also treats every Kiln
warning as an error, so a hand-edited output fails the check.

## Generators That Use Generators

A generator is an ordinary Wado package, so its own source may import a
generated module. Here the outer generator's source imports what the inner one
produces from `value.txt`:

<!-- {"source": "wado-cli/tests/fixtures/kiln_nested/src/outer.wado"} -->

```wado
use { answer } from "./value.txt"
    with {
        generator: { module: "./inner.wado" },
    };
```

- The inner invocation runs first, since the outer generator is built from its
  output.
- An inner clause names a generator the way an outer one does. A `lib:` name is
  resolved against the `[build-dependencies]` of the generator's own package.
- An invocation is placed relative to its package root, however it is reached.
  One schema has one output directory, whether a program or a generator imports
  it.

Loam is built this way. It reads a binary ONNX model through the types Grog
generates from `onnx.proto`:

<!-- {"source": "package-loam/src/onnx_proto.wado"} -->

```wado
use onnx from "./onnx.proto"
    with {
        generator: { module: "lib:grog" },
    };
```

and ONNX's text format through the parser Gale builds from a grammar:

<!-- {"source": "package-loam/src/onnx_text.wado"} -->

```wado
use onnx from "../grammar/Onnx.g4"
    with {
        generator: { module: "lib:gale" },
    };
```

Its own manifest lists both:

```toml
[build-dependencies]
"lib:gale" = { path = "../package-gale", package = "wado-lang:gale", version = "^0.0.28" }
"lib:grog" = { path = "../package-grog", package = "wado-lang:grog", version = "^0.0.28" }
```

An invocation whose generator is built from its own output, directly or through
other invocations, is a cycle. The cycle is an error naming the invocations in
it. This generator imports a module it would itself produce:

<!-- {"source": "wado-cli/tests/fixtures/kiln_nested_cycle/src/cyclic.wado"} -->

```wado
use { answer } from "./loop.spec"
    with {
        generator: { module: "./cyclic.wado" },
    };
```

## Runtime Libraries

Generated code may call a library that the generator's package also ships. The
generated module imports it like any other dependency, so the consumer lists
the package under `[dependencies]` as well as `[build-dependencies]`. Grog's
generated types are encoded by Grog's runtime:

```toml
[build-dependencies]
"lib:grog" = { path = "../..", package = "wado-lang:grog", version = "^0.0.28" }

[dependencies]
"lib:grog" = { path = "../..", package = "wado-lang:grog", version = "^0.0.28" }
```

The consumer imports the runtime and the generated types side by side:

<!-- {"source": "package-grog/tests/roundtrip/roundtrip_test.wado"} -->

```wado
use grog from "lib:grog";
use { Account, AccountAddress, AccountContact, Spread, SpreadPick, Status } from "./demo.proto"
    with {
        generator: { module: "lib:grog" },
    };
```

Only the use site knows what the consumer calls the package, so the generated
code imports it through `req.module`. `core:kiln`'s `package_of` takes the
package part of that name, and is `None` for a path, which names no package.
Grog refuses a path:

<!-- {"source": "package-grog/src/generator.wado"} -->

```wado
fn runtime_of(module: &String) -> Result<String, Error> {
    return package_of(module).ok_or(Error::Unsupported(
        `grog: invoked by the path ${module}; name it by the package spec its runtime is a dependency under`,
    ));
}
```

The name is among what a generator depends on, so renaming the dependency runs
the generator again.

## Dialects

A dialect is a superset of Wado, or another surface for it, implemented as a
generator. Its files take their own extension, since a `.wado` file is always
imported as a module. The generator's output is ordinary Wado on disk, and every
tool that reads Wado reads it unchanged.

Rationale: [WEP: Kiln, Dialects](./wep-2026-04-12-kiln.md#dialects).
