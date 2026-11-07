# Loam on TPU and WebGPU

This is the design for running Loam at the best speed both on a TPU and on
WebGPU, for one request at a time and for many requests served at once. It
starts from what sets the speed on each device. It changes decisions
[WEP: Loam](../docs/wep-2026-09-20-loam.md) made, and
[What this changes](#what-this-changes) names each one.

A TPU and WebGPU have about the same priority. A trade-off between them that
cannot be resolved goes WebGPU's way.

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

### Why a sequence of kernel calls cannot reach this

The WEP's seam is a generated sequence of kernel calls over a `Backend` trait,
run on every call, its kernels fused by the generator for WebGPU's dispatch
count. So:

- A whole-program compiler gets the graph back only by recording the calls and
  undoing the generator's fusion. The graph existed at build time, and the
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
axes. A tensor is `G::Tensor<E, Axes>`, a node of the graph being built.

- A device backend's `Graph` records the nodes. The runtime runs `build` once
  per shape class and hands the recorded graph to the device's compiler.
- `Cpu`'s `Graph` computes each node as it is added. So `Cpu` runs the same
  body as the devices and stays the reference they are held to.
- `WebGpuBackend` does both. It computes each node as it is added, through its
  device kernels, and it records a pass to compile it.

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
as GPT-2 spells its attention, is matched by the generator as a subgraph, so
`loam dump` reports it, and every backend takes the composite. The quantized
product and the mixture of experts arrive with the roadmap items that bring
their operators.

A subgraph matches only where the composite computes what it says. GPT-2 hides
a position through a table cut to the queries' rows, so the generator reads the
table and matches it only where every row is true on and below its diagonal. No
table reaches the device then, and no `gather` cuts it each pass. The keys must
be the queries' own batch and sequence, no broadcast may widen the scores past
the composite's, and nothing outside the subgraph may read a tensor inside it
but the presents.

A matched subgraph is the composite's decomposition as it stands, with every
choice it makes kept: a scale applied by `Div` or by `Mul`, a mask of f32's
lowest value, as GPT-2's is, rather than negative infinity, and that mask before
the bias rather than after it. A padded position reads negative infinity past
the composite's `reach`, as the softmax of the parts does. So `Cpu` computes what
the graph says. A kernel may differ from its decomposition only in what
[WEP: Loam § Results Are Held to a Tolerance, Not to the Bit](../docs/wep-2026-09-20-loam.md#results-are-held-to-a-tolerance-not-to-the-bit)
already allows: the order of a sum, a fused multiply-add, and an online
softmax's rescaling, which reorders a sum. A kernel that replaced the mask by
negative infinity, or skipped a `Cast`, would compute another function, so it
does not.

### Each device compiles the program its own way

#### PJRT

The compiler writes the graph as StableHLO text and compiles it through
`core:pjrt`. No Loam fusion runs; the composites are decomposed, since XLA
fuses them again. The prepared weights and the cache are parameters, the cache
donated. A product's `precision_config` is an option of the backend, and
`DEFAULT` unless the caller sets it, which on a TPU is faster than `HIGHEST`
and holds less of f32's precision. A compiled executable is serialized and kept
on disk, keyed by the program's text, the plugin and the topology, so a second
process compiles nothing.

`core:pjrt` mirrors the PJRT C API: compile, a buffer from and to the host, an
execution with donated inputs, and the compiled program's memory and cost. It
is not `wasi:`, which is reserved for WASI, and PJRT is no WASI proposal. The
host is `wado-run-pjrt`, in the repository's workspace: it opens a vendor's
plugin at run time, so unlike `wado-run-webgpu` it adds no heavy build. The same
backend reaches NVIDIA GPUs through XLA's CUDA plugin.

#### WebGPU

Loam's device compiler is part of the runtime library, so it ships in every
program that uses WebGPU. Compiling ahead of time for a list of device profiles
would multiply the variants by the profiles, which
[research §11](../docs/research-ml-inference-webgpu.md#11-backend-abstraction)
says does not scale. The compiler takes the graph and the device's features,
and makes a pass that runs from a list of dispatches:

- It fuses, by the rules [Loam on WebGPU](./webgpu.md) measured: elementwise
  chains, row groups, epilogues, prologues, products in pairs and
  concatenated products. It does so over the graph rather than in the
  generator, since some rules depend on the device, such as whether a prologue
  fits workgroup memory.
- It shares the work of one graph's shape classes, since its shaders take
  extents as parameters. A TPU compiles each class anew, since XLA compiles
  static shapes.
- It runs a composite as its kernel where one exists for the device.
- It picks each kernel's variant for the shape class's extents.
- It creates the pipelines, and gives each launch buffers, parameters and bind
  groups that later calls reuse.

A call writes the parameters that changed, encodes the list (pipeline, bind
group, dispatch, and nothing else), and submits it. WebGPU offers no reusable
compute command buffer, so the list is encoded on each call, from objects that
already exist.

### An extent that varies is a capacity

An extent that varies between calls is padded to a capacity, and its length
within the capacity is a value: a scalar input of the program. The runtime
picks the capacity from a list the caller gives, or by default the next power
of two or one and a half of one (1, 2, 3, 4, 6, 8, 12, …). The half steps
bound the padding to a third of what a pass computes, for twice the shape
classes. A capacity of 0 is its own class, so a prompt with no past compiles
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
- A merge of a padded axis behind an axis before it, as GPT-2's projections
  merge the rows and positions for a `Gemm`, puts each row's padding between
  the rows. The merged axis may pass through operations that compute each of
  its elements apart: an elementwise operation, a transpose, and a product
  that keeps it among its left operand's rows. A regrouping that splits it
  back as it was merged restores the prefix. Anything else that reads it, or a
  graph output that keeps it merged, makes the generator refuse the length.
- An extent that may fall as a length rises, such as a `Range` by the length,
  would hold fewer elements at the capacity than at the length, so the
  generator refuses to pad that length.
- An attention that is not causal reads every key, the padded ones too, so
  the generator refuses the lengths it reads.
- A length the graph owes equal to a sum with a refused length is refused
  too, since its capacity would not match the refused length's exact extent.
- Any other operation that reads across a padded axis, such as a split of it
  into axes it never lines up with, makes the generator refuse to pad that
  axis. It then compiles one program per length of that
  axis, and `loam dump` says which node refused.

A valid length is one per row of the batch axis, since the rows of one pass are
different requests (see
[Requests share a pass through slots](#requests-share-a-pass-through-slots)).
So each mask, count and write above reads its row's own.

The rows run along the first axis of every input that pads a length behind it.
A value computed from each row's lengths, as GPT-2's position ids are, holds
the rows of `[Slots]` ahead of its axes. An elementwise operation, a reshape
and a gather by it keep them there, and an elementwise operation whose output
has the batch axis folds them into it. A `Slice` from a valid length gathers
each row's window from that row's start. Where a node takes one length for every
row, as a `ReduceMean`'s divisor does, reads
how many rows the batch holds, or reads such a value any other way, the rows
share their lengths, and `loam dump` says which node made them.

The generated module takes them so:

- `build` takes `lengths: &Lengths<G>`, a value for each padded length it reads
  as a value: of `[Slots]` where each row holds its own, either one per row or
  one every row shares, and of `[]` where the rows share them. A build that
  reads none takes no `lengths`. Every extent the inputs give is a capacity. A
  condition the graph owes over a padded length is not checked there, since a
  capacity need not meet what its length meets.
- `compute` takes the inputs on the host. It checks them, and what the graph
  owes, against their own lengths. It pads each padded length to its
  capacity, `capacity_of` of it, builds, and cuts the outputs back to their
  lengths. A graph with a cache passes it to `compute` too, as the next
  section says.

An `If` whose condition the shape class decides is chosen when the graph is
built. One whose condition depends on a value is a `case` with both branches
on PJRT. On WebGPU one on an input takes its branch on the host, and any other
reads its condition back and runs the branch it names.

### The cache is a buffer of fixed capacity, written in place

A field of `Cache` whose axis the past's length gives holds the capacity of
positions, and its valid length is a value. A step writes its new positions
with a `scatter` at each row's valid length, which is ONNX's `TensorScatter`
with its `write_indices`, and returns the cache: donated on PJRT, the same
buffer on WebGPU. A cache that outgrows its capacity moves to the next shape
class, copied once.

This builds on [the decoder's cache](./design.md#the-decoders-cache), which is
one value already.

The past's length is what the graph owes it to be given the other inputs'
lengths: Gemma's attention owes the total to be the past and the sequence, so
the past is the mask's length less the ids'. `compute` fits the cache to the
present's capacity, `capacity_of(past + sequence)`, before the pass and after
it. A graph that owes its past's length no such equality gets no `compute`.

### Requests share a pass through slots

A pass has a fixed number of slots, and each slot is one row of the batch
axis. Each slot has its own cache, position and valid length, and a step
advances every slot in one pass, so the device reads each weight once for all
of them.

Where the rows hold their lengths apart and one past grows through a step, the
`decode` option also offers `decode_rows` and `Requests`:

- `decode_rows` is `decode` at a past for each row. The mask's extent is one
  capacity for every row, and each row's lengths are its own past's. A row's
  draws are keyed as row 0 at its own position, so a request draws in a slot
  what it draws alone.
- `Requests::new` holds `slots` empty slots of `capacity` positions.
- `place` runs a request's prompt alone through `compute`, writes its cache into
  its slot's row, and returns its first token.
- `decode` runs every slot's steps in one call, an empty slot's among them,
  whose tokens belong to no request.
- `release` empties a slot.

Which request takes which slot, and when, is the caller's scheduling. Loam
provides the pieces and does not schedule.

ONNX exports already carry what a row needs. `GroupQueryAttention` takes a
length for each row, and other decoders take an attention mask and position ids
for each row.

### The loop around the model runs on the device

The generator's `decode` option offers a decoding program beside `compute`.
`decode` takes one token a row, composes the model with a sampler, and feeds the
token the sampler picks back into the next step, for a number of steps the
caller gives. It returns the token ids and the cache, and reads the ids back
once. `Graph::repeated` is the counted loop, and `compute` then takes `room`,
how many positions past its own its cache holds for the steps to come.

- The sampler is a graph too: the greatest logit, or a temperature, a top-k or a
  top-p, drawn from a table the host writes once a call. Each entry is
  `core:prng`'s `Squares64` of the seed, the position and the row, so a call's
  splitting of its steps changes no draw, and no device's random source is read.
  Rows at positions of their own are each keyed as row 0.
- On PJRT the steps are a `while` loop in one program, with one execution and
  one read back for all of them.
- On WebGPU the steps are the pass's launches encoded once per step in one pass.
  The sampler writes its token where the next step's gather reads it, as the f32
  it equals. A loop keeps `SUBMIT_EVERY`'s submissions, since
  [the device then runs while the host records](#a-loops-submissions).
- A slot that picks an end token keeps returning the first of the end tokens,
  and its cache goes on being written past the valid length, where nothing reads
  it. A call takes each row's last token, so a row a call ended, or that a host
  pick ended, stays ended. The loop is counted on every backend, and the host
  cuts the row at its first end token.

### The device compiler lays out the weights

How a weight is stored is the device compiler's choice, made per model and per
device, not per shape class, since every shape class reads the same weights.
On WebGPU that covers the layouts measured today, column by column, in pairs,
concatenated, and in bf16 where nothing is lost. On a TPU it is bf16 where
nothing is lost, and the graph's own type otherwise. `Model` holds the stored
weights without spelling their layout, so a backend can change a layout without
changing the generated module.

### A model holds its weights and its programs

The generated module's `Model<Bk>` holds the weights as the device stores them
and a program for each shape class it has met. The caller owns it, and
`compute` takes it by `&mut`, since a call may compile a program into it.

- A program binds the weights it reads, in the layouts the device compiler
  chose for them. Held in one value, a program can only meet the weights it was
  compiled against, and no run time check is needed for it.
- Dropping the model frees the weights, the programs and their memory
  together. A backend holds nothing past a call, so one backend serves any
  number of models without a key to tell their programs apart.
- `Model` is shared by every request; `Cache` is one request's, or a row each
  in `Requests`. The weights an expert routing loads stay in the model.
- On `Cpu` a program is empty, since `Cpu` computes `build` directly and stays
  the reference.

### What this changes

| WEP section                                 | Change                                                                                                       |
| ------------------------------------------- | ------------------------------------------------------------------------------------------------------------ |
| Context                                     | Serving many requests on a TPU joins inference on the user's device as a goal.                               |
| A Decoder's Cache Is One Value              | A field the past's length gives holds a capacity of positions and a valid length, and is written in place.   |
| The Backend Is a Trait                      | The trait builds a graph of StableHLO's operations. A device compiles it once per shape class.               |
| Fusion Is Decided by Operator Kind          | Fusion is the WebGPU compiler's, run over the graph at run time. The PJRT backend fuses nothing.             |
| The MVP Is Tier 1 Only                      | Tier 2 is built: capacities, a valid length for each, and a cache of compiled programs.                      |
| Weights Are Prepared Once                   | The device compiler chooses a weight's layout. `Model` holds the weights beside the programs that bind them. |
| A Branch Is a Block of `forward`            | A branch the shape class decides is chosen when the graph is built. Any other is a `case` on PJRT.           |
| The Generator Reports Every Fusion Decision | `loam dump` reports the build-time analyses. The device compiler's decisions are reported when it runs.      |

## Plan

1. [x] The graph.
2. [x] Capacities and valid lengths.
3. [x] `wado-run-pjrt` and the StableHLO writer.
4. [x] The WebGPU compiler.
5. [x] Attention matched from its parts.
6. [x] The loop on the device.
7. [ ] Slots. Finished when the tokens per second at several numbers of slots
   are recorded on the CPU plugin and on `WebGpuBackend`, each request's
   tokens unchanged from its run alone.
8. [ ] A TPU. Finished when Gemma 3 picks onnxruntime's tokens on a TPU VM, and
   its time per token at one slot and at many is recorded beside
   `optimal_seconds`.

## Measurements

### The graph on the CPU

Gemma 3 (270M) picking 8 greedy tokens a prompt on `Cpu`, the test CPU time of
each entry, the weights' loading included:

| Entry                                        | Ratio to `compute` |
| -------------------------------------------- | ------------------ |
| `compute`, the graph's ops at the capacities | 1                  |
| `decode`, a step a call                      | 1.03               |

`decode` is the one test of `example/gemma3-270m/generate_test.wado`, and
`compute` is that test with `next_tokens` never taking its `decoded` branch:

```sh
cd package-loam/example
wado test --collector drc gemma3-270m/generate_test.wado
```

### Gemma 3 on the devices

Gemma 3 (270M) continuing the two prompts `generate.json` records by 32 tokens.
Each entry picks onnxruntime's tokens on both prompts, at `-O2`. Times are for
each token after the prompt's first, and a range spans the two prompts. WebGPU
is an RTX 4060 Ti under Vulkan on the release build of `wado-run-webgpu`, and
PJRT the `dev` build on XLA's CPU plugin. onnxruntime is `generate.mjs` on
`onnxruntime-node`, on the CPU and on its WebGPU provider, which keeps the
attention cache on the device and reads back the logits.

| Entry                   | Time a token, onnxruntime on the CPU as 1 | Device runs a token | Read backs a token |
| ----------------------- | ----------------------------------------- | ------------------- | ------------------ |
| WebGPU, a step a call   | 0.12–0.16                                 | 207                 | 1                  |
| WebGPU, 31 steps a call | 0.11–0.12                                 | 210                 | 0.03               |
| PJRT, a step a call     | 6.1–10.7                                  | 1                   | 4.5–8.0            |
| PJRT, 31 steps a call   | 1.1–1.9                                   | 0.03                | 0.03               |
| onnxruntime on WebGPU   | 0.35–0.41                                 |                     |                    |
| onnxruntime on the CPU  | 1                                         |                     |                    |

`example/gemma3-270m/webgpu.wado` and `pjrt.wado` print each entry:

```sh
cd package-loam/example/gemma3-270m
wado run-webgpu -O2 webgpu.wado
wado run-pjrt -O2 --plugin libpjrt_cpu.so pjrt.wado
```

A device run is a compute shader's dispatch on WebGPU and a program's execution
on PJRT. On PJRT a step a call is the most a capacity is crossed: it reads back
the tensors of a cache that outgrows its capacity, which `compute` grows on the
host. Each new capacity's compile is included. 31 steps a call runs the program
the prompt's `room` left capacity for, so nothing crosses one.

On WebGPU about two thirds of a token's time at a step a call is the host
encoding, timed by the wait on `on-submitted-work-done`. The loop spares a read
back's wait and nothing the host encodes, and its gain reaches its limit near 7
steps a call. Over 100 tokens:

| Steps a call | Time a token, the least as 1 | Dispatches a token |
| ------------ | ---------------------------- | ------------------ |
| 1            | 1.16–1.53                    | 210                |
| 7            | 1.06                         | 210                |
| 32           | 1.03–1.06                    | 210                |
| 99           | 1–1.03                       | 210                |

[The benchmarks](./benchmark/README.md#gemma-3-270m) time the first prompt
against onnxruntime on CUDA and on WebGPU, and against llama.cpp.

### A loop's submissions

Gemma 3 (270M) over 8 tokens a prompt on the same device, under the `dev` build,
when `example/gemma3-270m/continued.wado` ran 7 steps a call. Each entry picks
onnxruntime's tokens:

| Entry                                             | Time a token, onnxruntime on the CPU as 1 | Dispatches a token | Read backs a token |
| ------------------------------------------------- | ----------------------------------------- | ------------------ | ------------------ |
| `compute`, the compiled program at the capacities | 0.15–0.23                                 | 203                | 1                  |
| `decode`, a step a call                           | 0.15–0.24                                 | 207                | 1                  |
| `decode`, 7 steps a call                          | 0.14–0.15                                 | 210                | 0.14               |
| `decode`, 7 steps a call, submitted once          | 0.21–0.22                                 | 210                | 0.14               |
| onnxruntime on WebGPU                             | 0.34–0.75                                 |                    |                    |
| onnxruntime on the CPU                            | 1                                         |                    |                    |

Submitting a loop's dispatches once costs more than it spares: the device then
starts only when the host has finished encoding, where `SUBMIT_EVERY`'s
submissions let the two overlap.

onnxruntime on WebGPU reads near 0.35 on the first prompt, and near 0.35 or
near 0.75 on the second from one run to the next.

### Gemma 3's device memory

The memory `nvidia-smi` reports in use while `example/gemma3-270m/webgpu.wado`
runs on the RTX 4060 Ti, above what it reports before the run, sampled every
250 ms. The peak's split between weights, tensors and the driver is not
measured:

| Entry                   | MiB |
| ----------------------- | --- |
| `build/model.onnx_data` | 133 |
| the run's peak          | 917 |

### Gemma 3 compiled for a TPU

`example/gemma3-270m/analyzed.wado` compiles the programs for a `v5e:1x1`
through `libtpu.so`, on a machine with no TPU. The prompt has 8 positions, and the
step's capacity holds 12:

| What the plugin reports                       | Prompt      | Step        |
| --------------------------------------------- | ----------- | ----------- |
| argument bytes                                | 603,922,432 | 604,069,888 |
| alias bytes                                   | 294,912     | 442,368     |
| temporary bytes                               | 17,399,808  | 16,580,608  |
| `optimal_seconds`, unmodeled operations aside | 1.07 × 10⁻³ | 8.62 × 10⁻⁴ |

Alias bytes are the whole cache: 36 tensors of 256 f32s a position. The weights
take 1,139,501,568 bytes in f32. Of those, 536,084,482 bytes are the ones bf16
holds without loss, and 67,332,616 stay f32. Every matrix that takes part in a
product is among the first. The second are the four rotary tables and the 111
vectors and scalars that bf16 would round. A topology's `optimal_seconds` is a
model, and it does not price 94 and 93 of each program's operations
([research §7](../docs/research-tpu.md#7-the-compilers-cost-model)).

## Known gaps

- Where a node takes one length for every row, the rows share their lengths,
  and the module offers no `Requests`.
- Only GPT-2's spelling of attention is matched. Any other, a mask that is no
  causal table among them, stays operations, which write the scores.
- `build` cannot check what the graph owes over a padded length, which only
  `compute` knows on the host. A caller of `build` vouches for it.
- `compute` takes no capacities from the caller, beyond the `room` of a model
  built with `decode`.
- A pass cuts its cache to the capacity its own call needs, so the `room` one
  call asked for lasts only through calls that ask as much. A later call that
  asks more copies the cache to a larger buffer again.
- `decode` reads the prompt's logits back to pick the first token on the host. A
  program over the prompt and its steps would need the prompt's capacity and the
  step's in one graph, and an `If` model's branches apart.
- `decode` takes every input but its ids and mask as it is, so it refuses a model
  with another input a step changes, such as `position_ids`.
- A loop has no early exit, so a row that ends keeps running to the last step of
  its call, on every backend.
- `Graph` has no running sum, so each filter of a drawn step takes `BISECTIONS`
  and one sums over the vocabulary, and the draw one for each bit of the
  vocabulary's size. A greedy step takes none.
- Backends add up a row's probabilities in their own order, so a draw landing
  within rounding of a token's edge can pick apart on two of them. Greedy cannot.
- On WebGPU a device-held `i64` is an index `arg_max` ranked, as the f32 it
  equals, so an id is below 2²³, where the sum of two is still exact. Only
  `Add`, `Subtract`, `Minimum` and `Maximum` run on one there, and only with a
  value below the same bound. Anything else reads it back.
- WGSL lets an implementation assume no infinity or NaN at run time, and a
  shader still starts a maximum at negative infinity and masks attention with
  it. Where the CPU keeps a NaN, `Relu` among them, the device may give a number.
- A loop on WebGPU copies what it carries to a new buffer of the pass at each
  step, and those stay until the pass's end. Each also takes a new bind group.
- GPT-2 (124M)'s `compute` on `Cpu` takes 1.9 times the kernel pass's time a
  token, and Gemma 3's 4 times
  ([measured](./webgpu.md#measurements)). `Cpu` reads each weight from a
  `List`, where the kernel pass read packed vectors from linear memory.
- On `Cpu` a pass still copies the cache it writes into, since `scatter`
  returns a new tensor.
- On `Cpu` a step of `decode` also runs the sampler's graph, which a greedy
  `compute` leaves to the host, so it takes longer than a `compute`
  ([measured](#the-graph-on-the-cpu)).
- A pass on WebGPU holds every tensor it computes at once, and the device memory
  in use climbed through Gemma 3's run
  ([measured](#gemma-3s-device-memory)). A memory plan would spare that. It
  waits for a device or a model that runs short of memory.
- No kernel has a variant for a device's features. `WebGpuBackend` requests
  none, so it uses neither `shader-f16` nor subgroups, and `WORKGROUP_BYTES` is
  WebGPU's default limit rather than the device's. It waits for a device or a
  model that a variant would serve. A half graph's `shader-f16` is the roadmap's
  [A half graph keeps its type](./TODO.md#roadmap).
- An `If` on a value on WebGPU reads its condition back, which submits the
  commands before it and waits for the device. So a step holding one cannot
  share a submission with the steps after it, and the loop on the device reads
  back once for each such `If`. Indirect dispatches would spare that. It waits
  for a model that takes such an `If` on every step.
- The first call of a shape class waits for its program to compile. At build
  time no user waits on the generator, and at first use one does. Nothing spares
  a first call on WebGPU. The cache on disk that would spare a second process on
  PJRT is not built, and neither is the backend's `precision_config` option.
- `compute` fits a cache to a new capacity on the host, so on PJRT a step that
  crosses a capacity reads the cache back and uploads it.
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
