# Kiln Generators

A `use` clause whose source is neither a `.wado` module nor a Wasm asset (`.wasm` / `.wat`) goes through Kiln, Wado's code generation step. A generator turns the input into ordinary Wado source, which is then compiled like any hand-written module. `.g4`, `.proto`, `.graphql`, `.wit`, and a Wado dialect's own extension all take this path. The `with { generator: { ... } }` clause names the generator.

The examples use Gale, a generator that builds a parser from an ANTLR4 grammar
([WEP: Gale](./wep-2026-03-02-gale.md)):

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

This one also hands the generator a supplementary input:

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

The literal after `from` is the primary input. It is a `./` or `../` path
resolved against the declaring file, like a local module import.

## `with { generator: { ... } }` fields

Each field has one type, and any other key or type is an error at the use site.

| Field        | Required | Meaning                                                                                                                                                                                           |
| ------------ | -------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `module`     | yes      | The generator: a `./` / `../` path to its source, or a `<namespace>:<name>[@<version>]` coordinate or `lib:<nick>` alias resolved against `[build-dependencies]`. A bare name is an error.        |
| `version`    | no       | Exact version of a coordinate `module`, for a file with no `wado.toml`. An error beside a path `module`, or when the manifest declares the generator.                                             |
| `registry`   | no       | Registry of a coordinate `module` (`oci://<host>[/<prefix>]`), under the same conditions as `version`.                                                                                            |
| `options`    | no       | Record literal whose shape matches the generator's exported `pub struct Options`. See [Options](#options).                                                                                        |
| `inputs`     | no       | Supplementary input paths (`./` / `../`) the generator cannot discover from the primary alone, such as a sibling lexer grammar. A schema that refers to other files lists every one of them here. |
| `output_dir` | no       | A `./` / `../` directory, resolved against the declaring file, that receives the generated files. Default `build/kiln/<synthesized-id>/` under the package root.                                  |

Every file a generator sees is named literally at the use site. There is no
glob and no directory listing, so the whole input set is known before any
generator runs.

## Binding the import

A generator emits exactly one entry module and zero or more supplementary
modules. The `use` binds against the entry module, under the ordinary
visibility rules; supplementary modules are ordinary Wado files that the entry
reaches with ordinary `use` statements. The entry module does not need to
exist before the first compile, because the generator runs before the import
is resolved.

A clause applies in the file that declares it. Another `use` of the same schema
in that file, with no `with` of its own, binds against the same entry. Another
file gets no binding from it.

Clauses are collected from every module the program reaches, so a module deep
in the graph can import a generated module of its own. A generator is an
ordinary Wado package, so its own source may import a generated module too. An
invocation whose generator is built from its own output, directly or through
other invocations, is a cycle, and the cycle is an error naming the invocations
in it.

Generated files are compiled exactly like hand-written source, so a generator
that emits invalid Wado fails with an ordinary error against the generated file
on disk. Only the diagnostics a generator reports itself point
into its input files.

## Errors

- A `use` of a file that is neither `.wado` nor a Wasm asset is
  `KILN_MISSING_WITH` when the importing file declares no `with { generator }`
  clause for it.
- A `use` whose generator produced no module for that schema is
  `KILN_NO_GENERATED_MODULE`. The compiler never falls back to parsing the
  schema as Wado.
- Two clauses that agree on `module`, `version`, `registry`, the primary input,
  `inputs`, `options`, and `output_dir` are one invocation, wherever in the
  program they appear.
  Two clauses that share a primary input but disagree on any of those are an
  error naming both.

## Options

A generator declares its options as `pub struct Options`, and every use site's
`options` is checked against it before the generator runs, so a typo or type
mismatch is reported on the offending key.

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

## Manifest

Generators are declared in `[build-dependencies]` of `wado.toml` (a build-only graph that does not enter the consuming project's runtime dependency graph):

```toml
[build-dependencies]
"wado-lang:gale" = { version = "^0.0.9" }
```

Generated code that calls a runtime library the generator's package ships
imports it like any other dependency, so the consumer also lists that package
under `[dependencies]`.

## Authoring a generator

A generator is a normal Wado package whose `wado.toml` maps the `core:kiln/generator` world to a module under `[world]`:

```toml
[world]
"core:kiln/generator" = "src/generator.wado"
```

That module exports the world's `generate` function:

<!-- {"fixture":"spec_modules_kiln_generator.wado"} -->

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
`fn generate(req: Request)`.

What a generator receives and returns:

- `req.primary` and `req.inputs` are `InputFile`s: the path as the use site
  wrote it, and the content as a `Stream<u8>`. `read_all` and `read_text`
  collect a whole file.
- `req.module` is how the use site named the generator: the `module:`
  specifier as written, or for a relative path, that path from the project
  root. Output that imports a library the generator's package also ships names
  it through this, since only the use site knows what the consumer calls that
  package.
- `req.options` is the use site's options, with defaults filled in.
- It returns a `Response` whose `files` are `OutputFile`s: a path relative to
  the output directory, the Wado source, and whether this file is the entry
  module. Or it returns an `Error`: `InvalidSchema`, `Unsupported`, or
  `Other`, each with a message.
- It may report diagnostics through the `KilnHost` effect, each optionally
  spanning a byte range of one of the files it received. They surface as
  ordinary compile diagnostics.

An `Options` field is one of `bool`, a fixed-width integer, `f32`, `f64`,
`String`, a payload-less `enum`, a non-recursive `struct` of such fields,
`Option<T>`, `List<T>`, or `TreeMap<String, V>`. A field default is a literal.
Any other field fails the generator's own compile, since no use site could
supply it.

A generator runs in a deterministic sandbox: no clocks, randomness, network,
environment, or filesystem. Every input arrives by value, listed at the use site,
and every output is returned in the response. A generator that imports a
`wasi:*` interface, directly or through a `core:*` module, is a compile error
(`KILN_GENERATOR_FORBIDDEN_IMPORT`).

Outputs are written to the invocation's output directory, each stamped with a
`#![generated(by = "...", sources = [...])]` header naming the generator and
its inputs. A file in that directory that carries the header belongs to the
invocation, and a later run may overwrite or remove it. A file without it is
left alone.

Rationale: [WEP: Kiln](./wep-2026-04-12-kiln.md).
