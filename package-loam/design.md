# Loam Design

[WEP: Loam](../docs/wep-2026-09-20-loam.md) holds the design up to its
completion, and this file what was built after it. These documents hold a part
each:

- [Loam on TPU and WebGPU](./devices.md): running at the best speed on both
  devices.
- [Loam on WebGPU](./webgpu.md): how a pass runs on a WebGPU device.
- [Loam Tokenizer](./tokenizer.md): the tokenizer.
- [Runtime Assets](./remote-import.md): files a program reads at run time, not
  settled.

A roadmap item in [TODO.md](./TODO.md) writes its design here when it is
finished. Where it changes a decision the WEP made, it names the WEP's section.

## The Probe

A build names a model's whole checkpoint and reads its header alone, as The
Graph Is the Schema decided. Loam's generator exports `probe`. A `safetensors`
input's extent runs through the end of its header, whose length its first eight
bytes give, so the probe reads those eight. Every other input, a graph or a
tokenizer, is schema throughout, and its extent is the whole file.

A Kiln host serves the probe's stream a range at a time, then reads each input
to its extent and no further, for `generate`, the cache key and the cache check
alike. `wado test`'s result cache and the run's source watch record such an
input as the prefix it is. The protocol is in
[the Kiln spec](../docs/spec-kiln.md#the-probe).

What a build reads of the checkpoint `tools/onnx_split.wado` writes for each
model, counted with `strace -f -y -e trace=read,pread64` over
`wado check [--no-cache]` of a module naming it, summing the reads of the
`.safetensors` file:

| Checkpoint     | File size     | Build that runs the generator | Build that hits the cache |
| -------------- | ------------- | ----------------------------- | ------------------------- |
| GPT-2 (124M)   | 652,163,888   | 15,160                        | 15,152                    |
| Gemma 3 (270M) | 1,139,529,588 | 28,016                        | 28,008                    |

A build that runs the generator reads the eight bytes the probe asks for, then
the header.

### The examples

`example/gpt2-124m/` and `example/gemma3-270m/` import the model Hugging Face
publishes by its URL, pinned by its SHA-256, and read its weights in place as
[Weights read in place](#weights-read-in-place) says. Nothing is split or
committed. A program reaches no file its build read, so each `fetch.sh`
downloads the file the weights load from. Building a model downloads it, so
`wado test` leaves the files naming one out.

## Inferred Axis Names

Every axis takes its name as
[WEP: Loam § Axis Names Are Inferred From the Graph](../docs/wep-2026-09-20-loam.md#axis-names-are-inferred-from-the-graph)
says, which also settles which name a class takes where several reach it.
MNIST-12, the tiny GPT-2's three graphs, GPT-2 (124M) and Gemma 3 (270M) all
build from the graph alone, and Loam takes no option that names an axis. An
example that wants a word of its own renames the axis where it imports it.
`loam dump` opens with the axes, each beside where it took its name: a
`dim_param`, or the tensor and position it was minted from.

## The Decoder's Cache

`Cache<Bk>` is built as
[WEP: Loam § A Decoder's Cache Is One Value](../docs/wep-2026-09-20-loam.md#a-decoders-cache-is-one-value)
decided, but for two departures.

The WEP stops the build at a past no present pairs with, or the reverse. Such
a tensor is not a mistake in the export: a decoder's encoder keys are pasts
that no step replaces, and `past_sequence_length` is a past of one number. So
only the names that pair are the cache, and the rest stay inputs and outputs.

The WEP's `Cache::empty(&mut backend)` takes nothing but the backend. Most
exports leave the batch open, so `empty` also takes the length of each axis the
graph leaves open beside the past's own, as
`Cache::empty(&mut backend, batch_size)`. A graph whose `dims` fix every such
axis keeps the WEP's signature.

`compute` retains the cache before it begins. A caller that drives `build`
itself calls `retain_fields(&mut backend, &cache)` there.

## Weight Sources

[WEP: Loam § The Graph Is the Schema, the Checkpoint Is Data](../docs/wep-2026-09-20-loam.md#the-graph-is-the-schema-the-checkpoint-is-data)
gives the loader one `ByteList`. A model larger than the guest's memory cannot
be one, so the loader reads from a source instead, a range at a time:

```wado
Params::load<affine Src: WeightSource>(backend: &mut Bk, source: &Src) -> Result<Params<Bk>, LoadError>
```

`WeightSource` reads a byte range of a file, by the name the build gave it, and
says how long a file is. The generated module names every file it reads, so one
model may spread its weights over several, and the loader holds one weight's
bytes at a time. Before any weight is read, each file is checked: a
safetensors file's header, a model file's length, and an external data file's
length against the last byte the graph places in it. So a file that is missing
or too short stops the load before a weight reaches the device. An error names
the file and the tensor.

A weight reaches the backend as a `HostWeight`: its element kind, its extents,
and the little-endian bytes the checkpoint stores, in linear memory. Each
backend lays its copy out from those bytes. `PjrtBackend` uploads them as they
are, and an f32 weight as bf16 where every value's low half is zero.
`WebGpuBackend` builds its matrix layout, packed in a half where one holds every
value, by reading values from the bytes. `Cpu` decodes them into a tensor. A
float or bool weight reaches a device with no GC list on the way; WebGPU decodes
a `u8` or `i64` one and keeps it as `keep` does.

Two sources come with the runtime. `FileSource { dir }` reads the files of a
directory, a range at a time. A `HashMap<String, ByteList>` holds each file
whole, by name, for a model small enough to hold or bytes embedded with
`#include_bytes`.

A build whose graph lacks a weight's data and is given no checkpoint knows no
file to read it from, so the module has no `load`, and the caller builds
`Weights` itself.

## Sharded Checkpoints

A large model is published as several safetensors files beside an index, in
Hugging Face's format: `model.safetensors.index.json` holds a `weight_map` from
each tensor's name to the file holding it. A use site gives the index and the
shards as inputs, the index under `type: "safetensors-index"`:

```wado
use { Model, Params, compute } from "./model.onnxtext" with {
    type: "onnxtext",
    generator: {
        module: "lib:loam",
        inputs: [
            { path: "./model.safetensors.index.json", type: "safetensors-index" },
            { path: "./model-00001-of-00002.safetensors", type: "safetensors" },
            { path: "./model-00002-of-00002.safetensors", type: "safetensors" },
        ],
    },
};
```

The index decides where each tensor is, so a tensor is looked up in its own
shard alone. The index's probe extent is the whole file, as a graph's is, and
each shard's is its header. A build refuses:

- two indexes, or several checkpoints with no index;
- an index that places a tensor in a path rather than a file name, since a
  shard sits beside its index;
- a tensor the graph leaves to the checkpoint that the index places in no
  shard;
- a shard whose file name the index never names, or two shards of one name;
- a tensor whose shard is not given, naming the shard.

Each tensor is then checked against its shard's header as against a single
file's, and a disagreement names the shard.

The generated module carries the placement, so the loader reads no index. It
reads each shard from the source under the file name the index uses.

`tools/onnx_split.wado --max-shard-bytes <n>` writes the shards and the index.
It fills each shard in the graph's order up to `n` payload bytes, and a tensor
larger than that goes alone.

### Weights read in place

An exporter writes a model past protobuf's 2 GiB as a graph whose initializers
name an external data file, each weight at an offset and a length in it. Hugging
Face publishes such a model as it is, so reading the file in place spares a
consumer the split into a checkpoint. The graph alone is the use site's file:

```wado
use { Model, Params, compute } from "./model.onnx" with {
    type: "onnx",
    generator: { module: "lib:loam" },
};
```

The build reads no byte of the data files. It takes each weight's file and
range from the graph, checked against the weight's shape, so the generated
module carries them, and the loader reads each weight from its file. The
weights may be spread over several files. A file shorter than a range is a
`LoadError` naming the tensor and the file. A build refuses a weight with no
length, a range past what a module can address, and a checkpoint given beside
them. `tools/onnx_split.wado --external-data` writes a model this way.

A model under 2 GiB is published with its weights in the protobuf, each in a
`raw_data` field. `embed: false` in the options leaves them there: the build
records where each weight's `raw_data` sits, and holds no payload it does not
read. The loader reads the model file itself, under the name the build read it
by, and checks its length against the one the build read before reading a
weight.

## Half Element Types

A graph's `float16` and `bfloat16` tensors are `f16` and `bf16` on every device,
as
[WEP: Loam § Wado Stores a Half, a Device Computes In It](../docs/wep-2026-09-20-loam.md#wado-stores-a-half-a-device-computes-in-it)
decided, with one departure on `WebGpuBackend`.

The WEP has a device with `shader-f16` compute in f16. `WebGpuBackend` reads
and writes f16s there, but computes in f32 between the two. A kernel widens
each half it reads, computes in f32, rounds what it stores with `to_f16`, and
stores the f16. A device without the feature reads two halves from each `u32`
and stores the rounded value as an f32. Both paths compute in f32. A product
whose operand holds halves reads two elements a word, which sums in another
order, so the two paths' results can differ by a rounding. Arithmetic in f16 would be faster on some devices, and would hold
the results to a tolerance instead. bf16 has no WGSL type, so it takes the f32
path on every device.

| Holds            | With `shader-f16` (f16) | Without, or bf16                     |
| ---------------- | ----------------------- | ------------------------------------ |
| A weight         | `array<f16>`            | two halves to a `u32`                |
| What it computes | `array<f16>`            | f32s, each equal to its rounded half |

`forgo_shader_f16` makes a backend compute as a device without the feature
does, so one device tests both paths.

A node of halves that takes several steps computes them in f32 and rounds its
result once, on every device: a mean, a softmax, an attention with its rotary
embedding, a normalization, a `Gelu`, and a `Gemm` or `Conv` through its scale
and bias. A step
rounded on the way would turn a value past a half's range into an infinity
that the next step could have brought back.

On the CPU, onnxruntime computes the float16 nodes of Gemma 3 (270M)'s `fp16`
export in f32. Loam rounds each node's result to f16, as the graph says, so
its logits sit apart from onnxruntime's while the tokens match. Each row is the
largest deviation of the first token's logits, on an NVIDIA RTX 4060 Ti over
Vulkan and on XLA's CPU plugin. Every row picks onnxruntime's tokens.

| Run, in `example/gemma3-270m/`                             | "Hello, my name is" | The France prompt |
| ---------------------------------------------------------- | ------------------- | ----------------- |
| `wado run --collector drc fp16_cpu.wado`                   | 0.0625              | 0.109375          |
| `wado run-webgpu fp16_webgpu.wado`                         | 0.09375             | 0.1484375         |
| `wado run-webgpu fp16_webgpu.wado -- --forgo-shader-f16`   | 0.125               | 0.1484375         |
| `wado run-pjrt -O2 --plugin libpjrt_cpu.so fp16_pjrt.wado` | 0.109375            | 0.078125          |

onnxruntime's greedy pick leads the next logit by as little as this, over every
step of every prompt, so a deviation above can exceed a pick's lead:

| Run, in `example/gemma3-270m/` | Smallest lead |
| ------------------------------ | ------------- |
| `node generate.mjs`            | 0.03125       |
