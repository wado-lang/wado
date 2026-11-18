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
safetensors file's header, and its length against the last byte the header
places a tensor in; a model file's length; and an external data file's length
against the last byte the graph places in it. So a file that is missing or too
short stops the load before a weight reaches the device. An error names the
file, and the tensor where one is to blame.

A weight reaches the backend as a `HostWeight`: its element kind, its extents,
and the little-endian bytes the checkpoint stores, in linear memory. Each
backend lays its copy out from those bytes. `PjrtBackend` uploads them as they
are, and an f32 weight as bf16 where every value's low half is zero.
`WebGpuBackend` builds its matrix layout, packed in a half where one holds every
value, by reading values from the bytes. `Cpu` keeps the bytes, as a device
keeps its copy, so a model's weights need not fit the GC heap beside what a
pass computes. A reshape decodes nothing, a gather the rows it takes, and any
other operation the whole weight, for as long as it reads it. A float or bool weight reaches a
device with no GC list on the way; WebGPU decodes a `u8` or `i64` one and keeps
it as `keep` does.

A `Cpu` reads only the weights it keeps, as a device does, so a model's
`Params` and the `compute` that reads them take the same `Cpu`.

`conformance/large/` shows a checkpoint larger than the guest loading. It writes
80 MiB of weights over two shards, a chain of permutations that move each
element where no other order of them would. Under `--max-memory 32m` the chain
loads onto `WebGpuBackend` and `PjrtBackend` and runs with every output in
place. `Cpu`, which keeps the weights whole in linear memory, loads them from
the same `FileSource` with no limit and runs out of memory under it. The
runners' tests run it.

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

| Run, in `example/gemma3-270m/`                           | "Hello, my name is" | The France prompt |
| -------------------------------------------------------- | ------------------- | ----------------- |
| `wado run --collector drc fp16_cpu.wado`                 | 0.0625              | 0.109375          |
| `wado run-webgpu fp16_webgpu.wado`                       | 0.09375             | 0.1484375         |
| `wado run-webgpu fp16_webgpu.wado -- --forgo-shader-f16` | 0.125               | 0.1484375         |
| `wado run-pjrt -O2 fp16_pjrt.wado`                       | 0.109375            | 0.078125          |

onnxruntime's greedy pick leads the next logit by as little as this, over every
step of every prompt, so a deviation above can exceed a pick's lead:

| Run, in `example/gemma3-270m/` | Smallest lead |
| ------------------------------ | ------------- |
| `node generate.mjs`            | 0.03125       |

## Quantized Weights

onnxruntime's `MatMulNBits` and `GatherBlockQuantized` read weights packed 2, 4
or 8 bits to a value, in blocks that share a scale and a zero point. A
quantized weight is the tensors the operator takes: the packed `u8`, the scales
and the zero points. The bit width and the block size are attributes of two
`Graph` composites, `quantized_product` and `quantized_gather`, so the weight
has no type of its own.

Each composite's decomposition is its meaning. `Graph` has no bitwise
operation, so it unpacks a byte by division: the low bits first, as
onnxruntime packs them. A block without a zero point is centered on
`2^(bits-1)`. A gathered index past either end of the table, after one wrap
from the end, gives a row of zeros, which onnxruntime gives as well.

No device unpacks a weight when it loads.

- `Cpu` dequantizes one row of the weight at a time, rounded as the
  decomposition rounds it and summed as `dot_general` sums, so it computes the
  decomposition's bits.
- `WebGpuBackend` holds a `u8` tensor four bytes to a word. The product runs
  one workgroup per output column and eight input rows, each lane unpacking
  every 256th byte of the column. The gather runs one invocation per output
  element. In a half graph each rounds a weight to the half, as the
  decomposition stores it, and its result once.
- `PjrtBackend` compiles the decomposition, which XLA fuses.

Each row is the largest deviation of the first token's logits from
onnxruntime's, for Gemma 3 (270M)'s `q4` export, on an NVIDIA RTX 4060 Ti over
Vulkan and on XLA's CPU plugin. Every row picks onnxruntime's tokens.

| Run, in `example/gemma3-270m/`         | "Hello, my name is" | The France prompt |
| -------------------------------------- | ------------------- | ----------------- |
| `wado run --collector drc q4_cpu.wado` | 0.0000777           | 0.000122          |
| `wado run-webgpu q4_webgpu.wado`       | 0.0000758           | 0.000104          |
| `wado run-pjrt -O2 q4_pjrt.wado`       | 0.0000820           | 0.000102          |

The `q4f16` export computes in halves, so its logits sit as far from
onnxruntime's as the fp16 export's. Every row picks onnxruntime's tokens.

| Run, in `example/gemma3-270m/`            | "Hello, my name is" | The France prompt |
| ----------------------------------------- | ------------------- | ----------------- |
| `wado run --collector drc q4f16_cpu.wado` | 0.109               | 0.0781            |
| `wado run-webgpu q4f16_webgpu.wado`       | 0.141               | 0.102             |
| `wado run-pjrt -O2 q4f16_pjrt.wado`       | 0.141               | 0.0703            |

onnxruntime's greedy pick leads the next logit by as little as this, over every
step of every prompt:

| Run, in `example/gemma3-270m/`          | Smallest lead |
| --------------------------------------- | ------------- |
| `node generate.mjs generate_q4.json`    | 0.290         |
| `node generate.mjs generate_q4f16.json` | 0.141         |

### Mixture of Experts

onnxruntime's `QMoE` runs each token through the `k` experts its row of router
logits ranks highest. Each pick weighs its expert's output by its softmax over
every expert, divided by the picks' sum where `normalize_routing_weights` is
set. onnxruntime divides whether or not it is set, and Loam follows the
definition. An expert is a SwiGLU between two `quantized_product`s, so
`WebGpuBackend` runs both products in its kernels.

`mixture_of_experts` in `runtime/experts.wado` takes one of two forms by the
extents, computing in f32 and rounding once to the graph's element type. With
fewer picks than experts, as in a decode step, it runs each pick on its token's
row. With more, as over a prompt, it runs each expert over every row, weighted
by a gate that is zero where the token did not pick it. Either form is a
`repeated` loop that gathers one expert's slices of the weights at a time, so
its temporaries hold one expert, never all of them.

What `PjrtBackend::analyzing("v5e:1x1")` reports for the two forms, over the
four experts of `graph_cases.wado`'s `experts_case`, through
`conformance/pjrt_topology.wado` under `mise run test-pjrt`:

| Form, over                   | Temporary bytes |
| ---------------------------- | --------------- |
| Each pick, a step of 1 token | 409,600         |
| Each expert, a prompt of 3   | 262,144         |

### Experts Held on Demand

A model's experts may not fit the device: gpt-oss-20b's take most of its
weights. `Model::load` takes a budget in bytes for them. Where every expert
fits, they are weights like any other, and nothing below happens. Otherwise the
device holds as many as fit, and each pass loads the ones the tokens route to
that it does not hold.

#### The host reads back the routing

Which experts a layer needs is known only once its router has run. The pass
records a host step there: `Graph::fetched` names the values the host reads and
the ones it gives back. `Programs::run` splits a recording at each host step
into programs of its own. It runs a program, reads the values the step names,
runs the step, and passes what the step gives to the programs after it. Only
the pass's own region may hold a host step, not a branch or a loop.

So a pass waits for the host once a layer, which
[WEP: Loam § The MVP Is Tier 1 Only](../docs/wep-2026-09-20-loam.md#the-mvp-is-tier-1-only)
says a `QMoE` spares it. That holds only where every expert fits, which a TPU
chip does for gpt-oss-20b and a GPU of 8 GB does not.

#### The pool

An expert is one slice along the first axis of each `QMoE` input indexed by
expert. The layers whose experts are laid out alike share a pool: one weight
for each of those inputs, whose first axis is the pool's slots, so a slot holds
one expert in each. The budget sets the number of slots, and each pool must
hold one layer's experts, since one prompt may route to all of them.

The host step of a layer reads its picks back, and loads each picked expert the
pool does not hold into the slot used longest ago, among those the layer does
not need. Each backend writes a slot in place, as it laid the weight out. It
gives the layer each distinct expert it picked and the slot holding it, and
the layer runs those experts alone.

`HeldExperts` keeps that store on the host. It counts the budget at the widths
the checkpoint stores. Each pool group first gets one layer's slots, and the
rest of the budget is shared in proportion to the bytes of the layers that
group still lacks. A budget below one layer per group is a `LoadError`. Where
a group's slots hold every layer, the slot of a layer's expert is fixed, so the
pass takes no host step for it.

#### What the generator pools

A `QMoE` is pooled where each expert weight it reads is an initializer that no
other operand, branch or output reads, stored where the graph's other weights
are. `Params` then holds a pool in place of those weights, and the layers whose
weights have the same elements and extents share it. `Model::load` and
`Model::embedded` take the budget; `Params::load` and `Params::embedded` hold
every expert.

A program after a host step must not read a weight the step has since written.
So `split` computes a value that reads only weights again in each program that
reads it, rather than passing it across the step.

`conformance/operators_test.wado`'s `QMoeLayers` runs two layers that share
pools holding half their experts, on `Cpu`. Over two layers onnxruntime sits up
to 4e-4 from the value computed in f64, so the case holds Loam to that value.
`held_experts.wado` holds each device's run of it to the same device's run with
every expert held. Its second token is ill-conditioned: a device parts from
`Cpu` there by as much as onnxruntime does, and the order in which a held run
sums its experts moves it by up to 6.3e-5.
