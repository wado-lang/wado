# Loam Design

This file holds the design of what was built after
[WEP: Loam](../docs/wep-2026-09-20-loam.md) was completed. The design before
that is in three documents:

- [WEP: Loam](../docs/wep-2026-09-20-loam.md), the design up to its completion.
- [Loam on WebGPU](./webgpu.md), the design of its WebGPU backend.
- [Loam Tokenizer](./tokenizer.md), the design of its tokenizer.

[Loam on TPU and WebGPU](./devices.md) is the design that runs Loam at its best
speed on both devices, which the roadmap builds.

A roadmap item in [TODO.md](./TODO.md) writes its design here when it is
finished. Where it changes a decision the WEP made, it names the WEP's section.

## The Probe

A build names a model's whole checkpoint and reads its header alone, as The
Graph Is the Schema decided. Loam's generator exports `probe`. A `safetensors`
input's extent runs through the end of its header, whose length its first eight
bytes give, so the probe reads those eight. Every other input, a graph or a
tokenizer, is schema throughout, and its extent is the whole file.

The probe is half of it. A Kiln host used to read every input whole and cut it
to the extent afterwards. It now serves the probe's stream a range at a time,
then reads each input to its extent and no further, for `generate`, the cache
key and the cache check alike. `wado test`'s result cache and the run's source
watch record such an input as the prefix it is. The protocol is in
[the Kiln spec](../docs/spec-kiln.md#the-probe).

What a build reads of each checkpoint, counted with
`strace -f -y -e trace=read,pread64` over `wado check [--no-cache] <model.wado>`
in `example/`, summing the reads of the `.safetensors` file, when the examples
named the checkpoint `tools/onnx_split.wado` writes:

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
downloads the file the weights load from.
[Runtime Assets](./remote-import.md) is the design that lets a program read
it from the cache instead. Building a model downloads it, so
`wado test` leaves the files naming one out. GPT-2's tokenizer test takes its
`Tokenizer` from the committed `tokenizer.json`, so it runs without a download.

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
decided, with three choices it left open.

- A field holds each past under the past's axes. A present's length axis is
  minted apart from the past's, so `pass` relabels each present to its past's
  axes with `reshape`, which moves no element, before it returns the cache.
- `Cache::empty` takes the length axis to be the one symbolic axis on which
  the past and its present differ, and gives it 0. Any other symbolic axis has
  no extent an empty cache could take, so the build stops with `OpenCache` and
  names it. The `dims` option fixes it, as `batch_size: 1` does in every
  example.
- `forward` retains the cache before it begins. A caller that drives `pass`
  itself calls `cache.retain(&mut backend)` there, as the Gemma 3 and GPT-2
  (124M) examples do.

The GPT-2 (124M) example builds from `decoder_model_merged.onnx`, whose graph
branches on whether it is given a past, so one set of weights runs the prompt
and each token after it. No line of the examples names a layer.

## Sharded Checkpoints

A large model is published as several safetensors files beside an index, in
Hugging Face's format: `model.safetensors.index.json` holds a `weight_map` from
each tensor's name to the file holding it. A use site gives the index and the
shards as inputs, the index under `type: "safetensors-index"`:

```wado
use { Prepared, forward } from "./model.onnxtext" with {
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
Prepared::load(backend: &mut Bk, shards: &HashMap<String, ByteList>) -> Result<Prepared<Bk>, LoadError>
```

The generated module carries the placement, so the loader reads no index. A
shard absent from the map is a `LoadError` naming a tensor the shard holds and
the shard. A single file's use site and loader are unchanged.

`tools/onnx_split.wado --max-shard-bytes <n>` writes the shards and the index.
It fills each shard in the graph's order up to `n` payload bytes, and a tensor
larger than that goes alone. `conformance/tiny_gpt2_sharded_test.wado` loads
the tiny GPT-2 from three shards and reproduces the logits of its single file.

### Weights read in place

An exporter writes a model past protobuf's 2 GiB as a graph whose initializers
name an external data file, each weight at an offset and a length in it. Hugging
Face publishes such a model as it is, so reading the file in place spares a
consumer the split into a checkpoint. The graph alone is the use site's file:

```wado
use { Prepared, forward } from "./model.onnx" with {
    type: "onnx",
    generator: { module: "lib:loam" },
};
```

The build reads no byte of the data file. It takes each weight's range from
the graph, checked against the weight's shape, so the generated module carries
the ranges and the loader is handed the data file:

```wado
Prepared::load(backend: &mut Bk, checkpoint: &Ck) -> Result<Prepared<Bk>, LoadError>
```

A file shorter than a range is a `LoadError` naming the tensor. A build
refuses a weight with no length, a range past what a module can address,
weights in two files, and a checkpoint given beside them.
`tools/onnx_split.wado --external-data` writes a model this way, and
`conformance/tiny_gpt2_external_test.wado` reproduces the tiny GPT-2's logits
from one.

A model under 2 GiB is published with its weights in the protobuf, each in a
`raw_data` field. `embed: false` in the options leaves them there: the build
copies the model field by field off its stream, records where each weight's
`raw_data` sits, and lets the payload stream past unheld. The loader is handed
the model file itself, and checks its length against the one the build read
before reading a weight. A constant's `raw_data` is copied, since the build
reads it, and so is any whose `data_type` comes after it, since the walk cannot
know it is a weight's until then. `conformance/tiny_gpt2_in_place_test.wado`
reproduces the tiny GPT-2's logits from its own `model.onnx` this way.
