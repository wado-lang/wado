# Runtime Assets

A design in progress for files a program reads at run time, such as a model's
weights. It is not settled: the open questions below block an implementation.

## Context

The examples import the model Hugging Face publishes by its URL, and the build
reads it into Wado's cache, pinned by its SHA-256. A program cannot read that
cache:

- A program reaches only the directories it is granted. A symlink from a granted
  directory into the cache is refused (`ELOOP`).
- GPT-2's weights are in the file the build read, so `fetch.sh` downloads the
  same 653 MB a second time.
- Gemma's weights are in `model.onnx_data`, which the build never reads. Nothing
  pins it but `fetch.sh`, and `Prepared::load` cannot tell it from another file
  long enough to hold every weight's range: it loads garbage. An `Asset` the host
  checked against its digest is the file the program named.

So each example's `fetch.sh` downloads the file its weights load from, and the
oracle scripts and benchmarks read the files it downloads too.

## Direction

### Declared in `wado.toml`

A package names each asset in `[assets]`, by a URL or a path:

```toml
[assets]
gemma-data = { url = "https://huggingface.co/onnx-community/gemma-3-270m-it-ONNX/resolve/2dbbfdb1b59bd034eb959428c6a7da9dd7ea27f0/onnx/model.onnx_data", integrity = "sha256:21ceb4e7effff33b74d22bb5587c57fd1dc611b3da84abf98145f48e6e5acebd" }
gpt2-model = { url = "https://huggingface.co/openai-community/gpt2/resolve/607a30d783dfa663caf39e06633721c8d4cfcd7e/onnx/decoder_model.onnx" }
dict = { path = "data/dict.bin" }
```

A URL without `integrity` is pinned in `wado.lock` the first time it is
fetched, as a [remote pin](../docs/spec-packages.md#remote-pins) is. The
manifest is where other tools find the files as well, such as the oracle
scripts.

### `#asset("name")` opens an `Asset`

`#asset("name")` opens the named asset and yields an `Asset`, which holds a
read-only `wasi:filesystem` descriptor. A name `[assets]` does not declare is a
compile error.

```wado
pub affine struct Asset { file: Descriptor }

impl Asset {
    pub fn len(&self) -> u64;
    pub fn read_at(&self, offset: u64, len: i32) -> Result<ByteList, FsError>;
    pub fn read_linear(&self) -> Result<LinearArray<u8>, FsError>;
    pub fn open(&self) -> Stream<u8>;
}
```

- No effect of its own. A WASI capability is the effect, so `#asset` requires
  `Preopens`, as `core:fs::root()` does, and the methods require nothing: holding
  the descriptor is the capability.
- `Asset` is affine, since it holds a resource. Dropping it closes the file.

### The host grants what the program names

`wado run`, `wado run-webgpu` and `wado test` compile from source, so the
compiler tells the host which assets the program names. Before instantiating it,
the host fetches each one into the cache and checks its digest. It then grants a
directory holding only those, hard-linked from the cache, read-only, under a
reserved name. The cache is keyed by digest, so a file a Kiln input already
fetched is not fetched again: GPT-2 downloads once.

### No package build

The artifact of a package build is a component, and the weights do not go in
one. A package declaring `[assets]` cannot be built or published as a package.
Such a program is distributed as its repository.

### Loam reads each weight where it is

An option names the asset, and the generated `Prepared::open(backend)` reads
each weight's range with `read_at`. The caller writes neither a path nor the
bytes:

```wado
let weights = gemma::Prepared::open(&mut backend)?;
```

Reading by range holds the weights once. `Prepared::load` given the whole file
holds the file and the weights at once. `load` stays for bytes the program
already has, such as `#include_bytes` in a test.

## Alternatives set aside

- The weights in the component, in a custom section. A section holds up to
  4 GiB and a component may have any number, but no instruction reads one, so a
  program still needs the host to hand the bytes over. wasmtime also hashes the
  whole binary for its compile cache on every run.
- The weights in a data segment, which wasmtime maps copy-on-write. A component's
  core module is at most 1 GiB (wasmparser's `MAX_WASM_MODULE_SIZE`), which
  Gemma's weights exceed.
- A Kiln `inputs` entry marked for run time. `inputs` are what the generator
  reads, and Gemma's data file is not one.
- A Wado interface of its own (`wado:assets`). A preopen grants the same with no
  new interface, and only `wado` hosts these programs.
- The program fetching over `wasi:http`. It downloads every run unless it is
  also granted somewhere to keep the file, and hashes a gigabyte in the guest.

## Open questions

- [ ] Whether a `use` can name an asset (`from "asset:gpt2-model"`), so GPT-2's
  URL is declared once for the build and the run.
- [ ] The assets of a dependency: whether a path or git dependency's
  `[assets]` are fetched for the program, and how their names are scoped.
- [ ] What `#asset` does where the host granted nothing, as under a host other
  than `wado`: trap naming the asset, or yield a `Result`.
- [ ] The reserved directory against `core:fs::root()`, which takes the first
  directory granted, and against `--no-dir`.
- [ ] What `wado compile` makes of a program naming an asset.
- [ ] The methods `Asset` carries.
- [ ] How another tool reads an asset: `wado fetch` for all of them, and a
  command printing one's path in the cache.
- [ ] Where the rules end up. Most of this is the compiler's and the CLI's, so
  it belongs in the specification once settled.
