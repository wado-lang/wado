# Loam Design

[WEP: Loam](../docs/wep-2026-09-20-loam.md) holds the design up to its
completion, and this file what was built after it. These documents hold a part
each:

- [Loam on TPU and WebGPU](./devices.md): running at the best speed on both
  devices, which the roadmap builds.
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
itself calls `cache.retain(&mut backend)` there.

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

[WEP: Loam § The Graph Is the Schema, the Checkpoint Is Data](../docs/wep-2026-09-20-loam.md#the-graph-is-the-schema-the-checkpoint-is-data)
gives the loader one `ByteList`. A sharded module's loader takes the shards
instead, keyed by the file names the index uses:

```wado
Params::load(backend: &mut Bk, shards: &HashMap<String, ByteList>) -> Result<Params<Bk>, LoadError>
```

The generated module carries the placement, so the loader reads no index. A
shard absent from the map is a `LoadError` naming a tensor the shard holds and
the shard. A single file's use site and loader are unchanged.

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

The build reads no byte of the data file. It takes each weight's range from
the graph, checked against the weight's shape, so the generated module carries
the ranges and the loader is handed the data file:

```wado
Params::load(backend: &mut Bk, checkpoint: &Ck) -> Result<Params<Bk>, LoadError>
```

A file shorter than a range is a `LoadError` naming the tensor. A build
refuses a weight with no length, a range past what a module can address,
weights in two files, and a checkpoint given beside them.
`tools/onnx_split.wado --external-data` writes a model this way.

A model under 2 GiB is published with its weights in the protobuf, each in a
`raw_data` field. `embed: false` in the options leaves them there: the build
records where each weight's `raw_data` sits, and holds no payload it does not
read. The loader is handed the model file itself, and checks its length against
the one the build read before reading a weight.
