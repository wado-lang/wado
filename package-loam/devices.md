# Loam on TPU and WebGPU

This is the design for running Loam at the best speed both on a TPU and on
WebGPU, for one request at a time and for many requests served at once. It
starts from what sets the speed on each device, not from the seam Loam has. It
replaces the earlier PJRT design, which kept that seam and traced it. It also
changes decisions [WEP: Loam](../docs/wep-2026-09-20-loam.md) made, and
[What this changes](#what-this-changes) names each one.

## Context

### What sets the speed on a TPU

A program reaches a TPU only as StableHLO, given to XLA through a PJRT plugin
([research §2](../docs/research-tpu.md#2-the-only-public-entry-is-pjrt)).
So the following hold:

- XLA fuses the whole program it is given. A fusion decision made before XLA
  sees the program buys nothing, and a program split into pieces leaves XLA
  less to fuse and pays a launch per piece.
- Every extent is fixed when the program compiles
  ([research §6](../docs/research-tpu.md#6-shapes-must-be-static)). A program
  compiles once per set of extents, and a compile is far slower than a run. So
  extents that change on every call, such as a decoder's position, must not be
  extents.
- A decoder's step over one token is bound by memory bandwidth. A product of
  one row by GPT-2's head does about 1.5 operations per byte it reads
  ([research §7](../docs/research-tpu.md#7-the-compilers-cost-model)). Adding
  rows adds operations and almost no bytes, so many requests in one pass cost
  little more than one. Serving many requests is where a TPU pays.
- A step's time is the weights' bytes over the bandwidth, so bf16 weights halve
  it beside f32.
- Each call to the device costs a launch and a wait. A decoder that returns to
  the host after every token pays both once per token.
- A buffer passed in and donated is updated in place. An attention cache that is
  not donated is copied on every step.

### What sets the speed on WebGPU

WebGPU has no graph compiler. Loam writes every shader and schedules every
dispatch itself, as [Loam on WebGPU](./webgpu.md) records.

- At one request, the dispatches a pass makes and the host's work to record them
  set the time
  ([research §5](../docs/research-ml-inference-webgpu.md#5-at-batch-size-1-dispatch-count-is-the-bottleneck)).
  Once those were cut, the device set Gemma 3's time.
- A read back is a synchronization. A decoder that reads its token back after
  every step stops the device once per token.
- The device decides which shader is fastest. Its limits, its subgroup size, and
  whether it has `shader-f16` are known only when the program runs.
- No barrier spans the device, so one dispatch cannot hold a whole model
  ([research §6](../docs/research-ml-inference-webgpu.md#webgpu-cannot-host-a-true-megakernel)).
- A step is bound by memory bandwidth here too, so a weight's storage width sets
  its cost.

### What the two share

Both devices are fastest under the same conditions:

- The work of a pass is decided before the call. XLA requires it. On WebGPU it
  takes the planning, the pipelines and the bind groups off the host's
  per-call path.
- What changes from call to call is data on the device: token ids, positions,
  valid lengths, which request a row belongs to.
- The loop around the model runs on the device: picking the next token, feeding
  it back, and taking several steps before the host reads anything.
- The attention cache stays in one place on the device and is written in place.
- Many requests share one pass.
- The backend chooses each weight's layout and width.

They differ in one thing: who fuses and schedules. On a TPU that is XLA. On
WebGPU it is Loam.

### Why the current seam cannot reach this

`forward<Bk: Backend>` is a sequence of kernel calls that runs on every call.
The generator's plan has already fused those kernels for WebGPU's dispatch
count. So:

- A whole-program compiler gets the graph back only by recording the calls and
  undoing the fusion the plan did. The graph existed at build time, and the
  seam threw it away.
- The trait's methods follow WebGPU's fusion: products with prologues,
  products in pairs, concatenated weights. Each new fusion adds methods that
  every backend must then implement.
- The host replans nothing, but it still walks the whole sequence on every
  call, one trait call per kernel.
- An extent that changes per call is an extent, so a compiled program would
  serve a single call.

## Decision

### A pass is a program, built once per shape class and run many times

A shape class is one capacity for every extent that varies (see
[An extent that varies is a capacity](#an-extent-that-varies-is-a-capacity)).
For each shape class and device, the runtime builds the pass as a graph,
compiles it, and keeps the compiled program. Each call then writes its inputs,
runs that program, and reads back what it returns. Nothing in the generated
module runs per operation on a call.

This is tier 2 of
[WEP: Loam § The MVP Is Tier 1 Only](../docs/wep-2026-09-20-loam.md#the-mvp-is-tier-1-only),
built: a cache of shape classes that fills on first use. A TPU makes it a
requirement, since XLA compiles at run time whatever Loam does. Capacities are
few by construction, so a model compiles a handful of programs, not one per
length.

### The generated module builds the program through a typed graph

The generator still emits Wado, and the ordinary compiler still checks every
call it generates against the axes. What the generated body does changes:
rather than computing, it adds nodes to a graph. `build<G: Graph>` takes the
inputs, the weights and the cache as nodes of `G`, and returns the outputs.

`Graph` is a trait whose methods are the operations below, each typed by its
axes as the kernels are today. A tensor is `G::Value<E, Axes>`, a node of the
graph being built.

- A device backend's `Graph` records the nodes. The runtime runs `build` once
  per shape class and hands the recorded graph to the device's compiler.
- `Cpu`'s `Graph` computes each node as it is added. So `Cpu` runs the same
  body as the devices and stays the reference they are held to.

`Holds` is what a backend and a graph share: a tensor type, `upload` and
`extents_of`. `Backend` and `Graph` both extend it, so `Cache<H>` and
`Passed<H>` serve the kernels' `pass` and the graph's `build` alike until the
WebGPU compiler replaces `pass`. The weights a graph reads are `Params<G>`, each
a tensor of the graph in the element type it computes in.

An operation whose output axes are not its operands' takes them as a type
argument, as `reshape` does, and the generator spells them. Wado's packs cannot
state where `dot_general`'s free axes go, so the type checks that each operand
is the tensor the generator meant, and the generator states the result.

The generator keeps its own analyses at build time: stage 0, axis inference,
which axes may be padded, and the cache's pairs. Each analysis is device
independent, and `loam dump` reports it as now.

### The operations are StableHLO's

The graph's operations take their meaning from the StableHLO specification:
`dot_general`, the elementwise operators, `reduce`, `select`, `broadcast_in_dim`,
`reshape`, `transpose`, `concatenate`, `slice`, `dynamic_slice`,
`dynamic_update_slice`, `gather`, `scatter`, `sort`, `iota`, `convert`,
`compare`, `case`, `while`, and `convolution` and `reduce_window` for a
convolutional network. A specification fixes what each operation means, which
Loam's own kernel set never did. The lowering to a TPU is then one to one. The
StableHLO front end, a roadmap item, reads into the same graph.

StableHLO has no error function, which GELU reads. `erf` takes its meaning from
CHLO, the dialect XLA lowers into StableHLO, as `chlo.erf`. It is the one
operation the TPU lowering cannot copy across, and how it writes one is open
until that step.

### A composite is a computation no fusion reaches

A composite is a node that names a computation and carries its decomposition
into the operations above, as StableHLO's `composite` does. A device that has a
kernel for the composite runs that kernel, and any other device runs the
decomposition. `Cpu` always runs the decomposition, so every kernel is held to
it. In `Graph` a composite is a method whose default body is the
decomposition, which a device with a kernel overrides.

A computation is a composite only where its fastest kernel computes in an order
that no fusion of its parts' kernels reaches. Where fusion reaches the fastest
kernel, the parts stay operations and the WebGPU compiler fuses them: a
normalization becomes a row group or a product's prologue, a gated layer a
pair of products, and attention's query, key and value one product. A rotary
embedding is elementwise steps over a gather.

| Composite          | What no fusion reaches                                                                                           |
| ------------------ | ---------------------------------------------------------------------------------------------------------------- |
| Attention          | An online softmax that never writes the scores. A product and a fold share no kernel, so fusion writes them.     |
| Quantized product  | Unpacking each weight inside the product's inner loop. No rule fuses an operator into a product's right operand. |
| Mixture of experts | Reading only the experts the routing picks.                                                                      |
| Sampler            | Choosing a token among the logits. Loam adds it to the graph; no model carries one.                              |

A TPU runs every composite as its decomposition. Its only hand-written kernels
are reached from Pallas, which runs in Python, and XLA fuses the decomposition
anyway. The composites pay on WebGPU.

#### What a composite means

A composite takes its meaning from the operator that names the computation,
ONNX's where ONNX has one, and its decomposition is that operator's
definition:

- Attention is ONNX's `Attention`. Its `nonpad_kv_seqlen` gives each row a
  valid length within a cache of fixed capacity, which is the cache this
  design holds.
- A quantized product is ONNX's `DequantizeLinear` with a `block_size`,
  feeding a `MatMul`.
- onnxruntime's operators map onto these. Its `GroupQueryAttention` is ONNX's
  `RotaryEmbedding`, a cache update, and `Attention`. Its `MatMulNBits` is a
  quantized product. Its `QMoE`, which ONNX has no operator for, is defined by
  onnxruntime's documentation, with onnxruntime as the oracle where that
  leaves a behavior open.
- The sampler is Loam's own, defined by its decomposition.

#### Where a composite is recognized

A composite comes first from an operator that names it: `Attention`,
`GroupQueryAttention`, `MatMulNBits`, `QMoE`, and `DequantizeLinear` with a
`block_size` before a `MatMul`. A graph that spells the computation in parts,
as GPT-2 spells its attention, is matched later, as a subgraph. The quantized
product and the mixture of experts arrive with the roadmap items that bring
their operators.

A matched subgraph is the composite's decomposition as it stands, with every
choice it makes kept: a scale applied by `Div` or by `Mul`, and a mask of f32's
lowest value, as GPT-2's is, rather than negative infinity. So `Cpu` computes what the graph says.
A kernel may differ from its decomposition only in what
[WEP: Loam § Results Are Held to a Tolerance, Not to the Bit](../docs/wep-2026-09-20-loam.md#results-are-held-to-a-tolerance-not-to-the-bit)
already allows: the order of a sum, a fused multiply-add, and an online
softmax's rescaling, which reorders a sum. A kernel that replaced the mask by
negative infinity, or skipped a `Cast`, would compute another function, so it
does not.

### Each device compiles the program its own way

#### PJRT

The compiler writes the graph as StableHLO text and compiles it through
`wado:pjrt`. No Loam fusion runs; the composites are decomposed, since XLA
fuses them again. The prepared weights and the cache are parameters, the cache
donated. A product's `precision_config` is an option of the backend, and
`DEFAULT` unless the caller sets it, which on a TPU is faster than `HIGHEST`
and holds less of f32's precision. A compiled executable is serialized and kept
on disk, keyed by the program's text, the plugin and the topology, so a second
process compiles nothing.

`wado:pjrt` mirrors the PJRT C API: compile, a buffer from and to the host, an
execution with donated inputs, and the compiled program's memory and cost. It
is not `wasi:`, which is reserved for WASI, and PJRT is no WASI proposal. The
host is `wado-run-pjrt`, a workspace of its own as `wado-run-webgpu` is, since
it loads a vendor's plugin the rest of the toolchain never needs. The same
backend reaches NVIDIA GPUs through XLA's CUDA plugin.

#### WebGPU

Loam's device compiler is part of the runtime library, so it ships in every
program that uses WebGPU, as the shader writer does now: `WebGpuBackend`
already writes each WGSL shader from its `Expr` when a pass first asks for it.
Compiling ahead of time for a list of device profiles would multiply the
variants by the profiles, which
[research §11](../docs/research-ml-inference-webgpu.md#11-backend-abstraction)
says does not scale. The compiler takes the graph and the device's features,
and makes a pass that runs from a list of dispatches:

- It fuses, by the rules [Loam on WebGPU](./webgpu.md) measured: elementwise
  chains, row groups, epilogues, prologues, products in pairs and
  concatenated products. It does so over the graph rather than in the
  generator, since some rules depend on the device, such as whether a prologue
  fits workgroup memory.
- It runs a composite as its kernel where one exists for the device.
- It picks each kernel's variant for the device's features and for the shape
  class's extents.
- It plans the pass's memory: one arena, each tensor at an offset, and a buffer
  reused once the tensor in it is dead.
- It creates the pipelines and the bind groups, and lays out one parameter
  buffer. The fields that change per call, such as positions and valid lengths,
  sit at known offsets in one block of it. A submission of several steps holds
  a block for each step, and each step binds its own by a dynamic offset.

A call writes those blocks in one write, encodes the list (pipeline, bind
group, dispatch, and nothing else), and submits it. WebGPU offers no reusable
compute command buffer, so the list is encoded on each call, from objects that
already exist.

### An extent that varies is a capacity

An extent that varies between calls is padded to a capacity, and its length
within the capacity is a value: a scalar input of the program. The runtime
picks the capacity from a list the caller gives, or by default the next power
of two. A capacity of 0 is its own class, so a prompt with no past compiles
apart from a step with one.

Padding must not change any value the caller reads. The generator decides for
each symbolic axis whether it can promise that, at build time:

- An elementwise operation, a regrouping that keeps the axis whole, and a
  reduction over another axis keep the padding apart from the valid elements.
- A reduction or a contraction over a padded axis reads the padding, so the
  compiled program sets the padding to the reduction's identity first: 0 for
  a sum and a contraction, 1 for a product, negative infinity for a maximum and
  a softmax, positive infinity for a minimum.
- A count of a padded axis is its valid length, never its capacity. A `Shape`
  of the axis yields the valid length, a value, and so does a `ReduceMean`'s
  divisor. A graph that computes from the count, as a causal mask's slice
  does, computes from the value. A slice whose start then depends on the value
  becomes a `gather` of each row's window, since each row has its own value
  and a `dynamic_slice` takes one start for all of them. A `gather` has no
  stride, so a slice of `length` elements with a step other than 1 takes
  `(length - 1) * step + 1` elements, and a strided `slice` follows it.
- A `Concat` along a padded axis would put padding between the parts, so it
  becomes a `scatter` that writes the second part at the first part's valid
  length.
- A merge of a padded axis with an axis after it keeps the valid elements a
  prefix.
- An extent that may fall as a length rises, such as a `Range` by the length,
  would hold fewer elements at the capacity than at the length, so the
  generator refuses to pad that length.
- Any other operation that reads across a padded axis, such as a merge with an
  axis before it, which would interleave the padding, makes the generator
  refuse to pad that axis. It then compiles one program per length of that
  axis, and `loam dump` says which node refused.

A valid length is one per row of the batch axis, since the rows of one pass are
different requests (see
[Requests share a pass through slots](#requests-share-a-pass-through-slots)).
So each mask, count and write above reads its row's own.

The generated module takes them so:

- `build` takes `lengths: &Lengths<G>`, a value of `[]` for each padded length
  it reads as a value. A build that reads none takes no `lengths`. Every
  extent the inputs give is a capacity.
- `compute` takes the inputs on the host. It checks them, and what the graph
  owes, against their own lengths. It pads each padded length to its
  capacity, the next power of two, builds, and cuts the outputs back to their
  lengths.

An `If` whose condition the shape class decides is chosen when the graph is
built. One whose condition depends on a value is a `case` with both branches.
On WebGPU each branch's dispatches are indirect: a kernel writes each branch's
workgroup counts from the condition, none for the branch not taken, and both
branches write the `If`'s outputs to one place. So no condition is read back,
and the pass stays one list.

### The cache is a buffer of fixed capacity, written in place

A field of `Cache` whose axis the past's length gives holds the capacity of
positions, and its valid length is a value. A step writes its new positions
with a `scatter` at each row's valid length, which is ONNX's `TensorScatter`
with its `write_indices`, and returns the cache: donated on PJRT, the same
buffer on WebGPU. A cache that outgrows its capacity moves to the next shape
class, copied once.

This builds on [the decoder's cache](./design.md#the-decoders-cache), which is
one value already.

### Requests share a pass through slots

A pass has a fixed number of slots, and each slot is one row of the batch
axis. Each slot has its own cache, position and valid length, which are
inputs, and an inactive slot is masked. A step advances every active slot in
one pass, so the device reads each weight once for all of them.

A prompt runs in a program of its own, of its own capacity. It writes its keys
and values into its slot's cache by `dynamic_update_slice` at the slot's index.
Which request takes which slot, and when a prompt runs, is the caller's
scheduling. Loam provides the pieces and does not schedule.

ONNX exports already carry what a row needs. `GroupQueryAttention` takes a
length for each row, and other decoders take an attention mask and position ids
for each row.

### The loop around the model runs on the device

The generated module offers a decoding program beside `forward`. It composes the
model with a sampler and feeds the token the sampler picks back into the next
step, for a number of steps the caller gives. It returns the token ids and
nothing else.

- The sampler is a graph too: the greatest logit, or a temperature, a top-k or a
  top-p, drawn by a keyed counter hash of the seed, the slot and the step.
  `core:prng`'s `Squares64` is one. The draw depends on no device's random
  source, so every backend picks the same token from the same logits.
- On PJRT the steps are a `while` loop in one program, with one execution and
  one read back for all of them.
- On WebGPU the steps are the pass's list encoded once per step in one
  submission. The sampler writes its token where the next step's gather reads
  it. Positions and valid lengths go up by one a step, so the host writes each
  step's block of the parameter buffer before it submits.
- A slot that picks an end token stops writing its cache and keeps returning
  the end token. The host discards what follows it.

### The device compiler lays out the weights

How a weight is stored is the device compiler's choice, made per model and per
device, not per shape class, since every shape class reads the same weights.
On WebGPU that covers the layouts measured today, column by column, in pairs,
concatenated, and in bf16 where nothing is lost. On a TPU it is bf16 where
nothing is lost, and the graph's own type otherwise. `Prepared` holds the stored weights without spelling their
layout, so a backend can change a layout without changing the generated module.

### What a call does

1. Check the inputs against the graph, as `forward` does now, and each valid
   length against its capacity.
2. Find the shape class's program, building and compiling it on first use.
3. Write the inputs and the per-call values.
4. Run the program.
5. Read back what it returns: token ids from a decoding program, the outputs
   the caller asked for from `forward`.

### What this changes

| WEP section                                 | Change                                                                                                     |
| ------------------------------------------- | ---------------------------------------------------------------------------------------------------------- |
| Context                                     | Serving many requests on a TPU joins inference on the user's device as a goal.                             |
| A Decoder's Cache Is One Value              | A field the past's length gives holds a capacity of positions and a valid length, and is written in place. |
| The Backend Is a Trait                      | The trait builds a graph of StableHLO's operations. A device compiles it once per shape class.             |
| Fusion Is Decided by Operator Kind          | Fusion is the WebGPU compiler's, run over the graph at run time. The PJRT backend fuses nothing.           |
| The MVP Is Tier 1 Only                      | Tier 2 is built: capacities, a valid length for each, and a cache of compiled programs.                    |
| Weights Are Prepared Once                   | The device compiler chooses a weight's layout. `Prepared` no longer spells it.                             |
| A Branch Is a Block of `forward`            | A branch the shape class decides is chosen when the graph is built. Any other branch is a `case`.          |
| The Generator Reports Every Fusion Decision | `loam dump` reports the build-time analyses. The device compiler's decisions are reported when it runs.    |

## Plan

1. [x] The graph. `Graph` and its operations in the runtime library, with the
   attention composite from `GroupQueryAttention` and `Attention`. The
   generated module builds through it, and `Cpu` computes
   through it. Finished when every conformance case, the tiny GPT-2 and Gemma 3
   (270M) pass on `Cpu` through `build`, and the CPU time per token is recorded
   as a ratio to today's ([measured](#the-graph-on-the-cpu)).
2. [ ] Capacities and valid lengths. The padding analysis, the masks it adds,
   and the cache of fixed capacity. Finished when Gemma 3 decodes on `Cpu`
   from padded inputs with onnxruntime's tokens, and `loam dump` reports each
   padded axis or the node that refused it.
3. [ ] `wado-run-pjrt` and the StableHLO writer. Finished when the tiny GPT-2,
   Gemma 3 and the conformance cases with an `If` run on XLA's CPU plugin
   within the tolerance of `Cpu`, with one
   compile per shape class. CI compiles the same programs for a TPU topology
   through `libtpu.so` and records `optimal_seconds`. There
   `PJRT_Executable_GetCompiledMemoryStats` reports argument bytes that hold
   Gemma 3's weights at half their f32 size, every product weight of it being
   bf16 without loss, and alias bytes equal to the cache's size.
4. [ ] The WebGPU compiler. Fusion, kernel variants, the memory plan and the
   recorded dispatch list over the graph. Finished when Gemma 3's time per token
   on `WebGpuBackend` is at most today's, with the host's time per token
   recorded beside the device's, the conformance cases with an `If` pass with
   no read back but the outputs, and the generator's plan and the fused trait
   methods are deleted.
5. [ ] Attention matched from its parts. Finished when GPT-2's attention runs
   as the composite on `WebGpuBackend` with onnxruntime's tokens, and its
   dispatches a layer are recorded beside the ten
   [Loam on WebGPU](./webgpu.md#what-a-layer-costs) counts.
6. [ ] The loop on the device. The sampler and the decoding program. Finished
   when Gemma 3 decodes several tokens per read back on both devices, with the
   tokens it picks one step at a time.
7. [ ] Slots. Finished when the tokens per second at several numbers of slots
   are recorded on the CPU plugin and on `WebGpuBackend`, each request's
   tokens unchanged from its run alone.
8. [ ] A TPU. Finished when Gemma 3 picks onnxruntime's tokens on a TPU VM, and
   its time per token at one slot and at many is recorded beside
   `optimal_seconds`.

## Measurements

### The graph on the CPU

Gemma 3 (270M) picking `generate.json`'s tokens greedily on `Cpu`, the test
CPU time of each entry, the weights' loading included:

| Entry                    | Ratio to `pass` |
| ------------------------ | --------------- |
| `pass`, the kernels      | 1               |
| `build`, the graph's ops | 0.85            |

Each is one test of `example/gemma3-270m/generate_test.wado`, run alone:

```sh
cd package-loam/example
wado test --collector drc gemma3-270m/generate_test.wado --test-name greedily
wado test --collector drc gemma3-270m/generate_test.wado --test-name "graph build"
```

## Known gaps

- A valid length is one value for every row, which is a batch of one, until
  slots (step 7) give each row its own.
- `build` checks what the graph owes against the capacities, not the lengths,
  so a condition a length meets and its capacity misses refuses the call.
- `compute` takes no capacities from the caller, and a graph with a cache has
  no `compute` until the cache is one of fixed capacity.

- The first call of a shape class waits for its program to compile. At build
  time no user waits on the generator, and at first use one does. The cache on
  disk spares a second process on PJRT, and nothing spares a first call on
  WebGPU.
- A model larger than one chip needs its weights split across several, and
  StableHLO states that through sharding. Nothing here places a tensor on more
  than one device.
- XLA's attention is its fusion of the decomposition. A TPU kernel written by
  hand is reached only from Pallas, which runs in Python.
- Text StableHLO carries no compatibility promise; only a portable artifact
  does, and writing one takes libStablehlo
  ([research §4](../docs/research-tpu.md#4-the-program)).
- `libtpu.so` aborts the process on an unknown extent rather than returning an
  error, so a host cannot recover from a program that carries one.
- No step here runs the CUDA plugin, so no run on an NVIDIA GPU is measured.
- The CPU plugin CI runs is ZML's build of XLA's `pjrt_c_api_cpu_plugin`. The
  XLA project publishes no CPU plugin.
