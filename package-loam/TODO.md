# Loam TODO

Open work on Loam. [`AGENTS.md`](./AGENTS.md) says how to work here, and
[`design.md`](./design.md) holds the design and names the documents it builds
on. A section name below, such as The Graph Is ONNX, is one of
[WEP: Loam](../docs/wep-2026-09-20-loam.md)'s.

The roadmap is ordered, and each item says what finishing it means. A known gap
says what is missing and what it admits, not how to close it. A finished item
leaves this file, and `design.md` states what it built.

## Roadmap

The roadmap leads to gpt-oss-20b, from onnx-community's `q4f16` export: a
graph beside seven data files, 12.6 GB together.

The devices keep in step. An item is finished on `Cpu`, `WebGpuBackend` and
`PjrtBackend` alike, the last under the CPU plugin. A run on a TPU is measured
beside the roadmap and holds no item up.

A weight stays as the checkpoint stores it. Unpacked to bf16, the model's 20
billion weights outgrow a TPU chip of 16 GB. Packed, they are under it.

What the model asks of Loam, as
`wado run tools/onnx_inventory.wado model_q4f16.onnx` reports it. The graph
holds no function and no branch. An operator below with nothing missing is one
Loam lowers, though the build stops before it reaches any of them.

| Operator                                       | Nodes | What Loam lacks                         |
| ---------------------------------------------- | ----- | --------------------------------------- |
| `GroupQueryAttention`, a window of 128 or none | 24    | The attention sinks, its twelfth input. |
| `QMoE`, 32 experts of 4 bits, 4 to a token     | 24    | The operator.                           |
| `SkipSimplifiedLayerNormalization`             | 48    | Nothing.                                |
| `MatMulNBits`, 4 bits in blocks of 32          | 73    | Nothing.                                |
| `Add`, `Reshape`, `Cast`, `Constant`, `Gather` | 103   | Nothing.                                |
| `ReduceSum`, `Shape`, `Sub`                    | 3     | Nothing.                                |
| `SimplifiedLayerNormalization`                 | 1     | Nothing.                                |

| Weights                    | Tensors | Bytes          |
| -------------------------- | ------- | -------------- |
| `float16`                  | 317     | 2,459,526,784  |
| `uint8`, packed int4       | 121     | 10,164,326,400 |
| In the graph               | 289     | 54,999,168     |
| In the largest data file   | 62      | 2,096,824,320  |
| One layer's `gate_up_proj` | 1       | 265,420,800    |

The embedding is a `float16` table a `Gather` reads, so the model has no
`GatherBlockQuantized`. The inputs and outputs are `int64` and `float16`, and
the two `Cast`s give `int32`.

1. [ ] The rest of gpt-oss-20b's operators: onnxruntime's `QMoE`, and a
   `GroupQueryAttention` with attention sinks. A `QMoE` decomposes
   in two forms. Over a prompt it loops over the experts and masks each one's
   rows, so its temporaries do not grow with the prompt. Over a step it gathers
   the experts the token routes to. Finished when `operators_test.wado` holds
   each to onnxruntime on every device, and `PjrtBackend::analyzing` reports
   the temporaries of both forms for a prompt and for a step.
2. [ ] Experts held on demand. An expert is one slice along the first axis of
   each `QMoE` input the operator's definition indexes by expert, so no model
   is named. A device holds as many experts as a budget the caller sets: all
   of them on a TPU chip, fewer on a GPU of 8 GB and on `Cpu`. The rest stay
   in the checkpoint until a token routes to one, and the guest's heap only
   passes an expert through. An expert the device does not hold is loaded,
   never computed in its place. Where the budget holds fewer than all the
   experts, the host reads back which experts a layer routes to before it runs
   them. So the host waits once a layer, which The MVP Is Tier 1 Only says a
   `QMoE` spares it. Finished when `operators_test.wado` holds a `QMoE` to
   onnxruntime with a budget of half its experts on every device.
3. [ ] gpt-oss-20b, a mixture of experts. Finished when
   `example/gpt-oss-20b/` continues a prompt with the tokens onnxruntime picks
   from onnx-community's `q4f16` export on `Cpu`, under `wado run-webgpu` on a
   device with 8 GB, and under `wado run-pjrt`, with no line that names a
   layer. `design.md` records how often a routed expert was already held, with
   the eviction order it chose and why, and what
   `PjrtBackend::analyzing("v5e:1x1")` reports for the model.

## Later

Work the road to gpt-oss-20b does not pass. It is unordered.

- [ ] A data file of 2 GiB or more. A range into a data file fits 32 bits,
  which each of gpt-oss-20b's seven does. Finished when a tensor stored past
  a data file's first 2 GiB loads on every device.

- [ ] TPU and WebGPU at their best speed. A pass is a graph built once per
  shape class and compiled for each device: StableHLO through `core:pjrt` for
  a TPU, Loam's own compiler for WebGPU. See
  [Loam on TPU and WebGPU](./devices.md), whose plan lists the steps, a TPU
  run and many requests at once among them. Finished when every step of that
  plan is.
- [ ] Narrowed outputs. A use site can name an output that each call reads at
  one position per row, and optionally only at some token ids. The call gives
  the positions and the ids. The generated module then computes only those
  values, as `outputs` already cuts the graph to what it reads (see The Graph
  Is ONNX). Picking each row's position moves above every node that works on
  each position alone. The ids pick entries along the head weight's
  vocabulary axis before its product. So the head's cost grows with the
  number of ids, not with the vocabulary, and no pass holds a logit for every
  position and token. Finished when the Gemma 3 example ranks the next token
  from the last position alone, and its logits at given ids lie within the
  tolerance of onnxruntime's at those ids. The `last` option does the first
  half: it fixes the position at the build, as the last one along an axis.
- [ ] Decisions. A decision asks a model one question about a state and
  reads the probability of each answer from one pass, rather than generating
  text. Loam's decision module takes any model that gives narrowed logits,
  and that model's tokenizer. The caller spells the model's chat turns. A
  question is a choice among options, given at run time. A yes-or-no question
  is a choice of two. A score is a choice among the points of a scale, and its
  answer also gives their expected value. Each option is labelled with a
  letter, so a choice holds at most 26. A label's id is the token the letter
  adds when it follows the prompt, so the chat turns decide whether that is
  `A` or ` A`. A letter that adds more than one token, or changes the
  prompt's own tokens, stops the call. The answer is the softmax of the
  logits at the labels. Finished when the Gemma 3 example answers a set of
  questions with probabilities within the tolerance of those onnxruntime's
  logits give.
- [ ] A state read by many questions. The state's tokens run once. Each
  question runs only its own tokens, against the state's cache, and leaves
  that cache as it was. Finished when `design.md` records the time several
  questions take against one state of 1,000 tokens as a ratio to the time
  with the state run for each, beside the commands that measured both, and
  each probability lies within the tolerance of the one it gets with the
  state run for it.
- [ ] Questions in one pass. The questions on one state run as the rows of
  one pass, so the device reads each weight once for all of them. A shorter
  question is padded at its end, and its answer is read at its own last
  token. Every row reads the state's cache, which keeps its batch of one
  rather than being copied for each row. The graph gives the past and the new
  tokens one batch axis, and `compute` checks that the inputs agree on it
  (see Axis Identity Is a Type, Extents Are Values). This item changes that
  check to admit a past of batch one beside n rows. Finished when `design.md`
  records the time n questions take as rows of one pass as a ratio to the
  time they take one after another, for several n and questions of different
  lengths, and each probability lies within the tolerance of the one its
  question gets alone.
- [ ] Calibrated decisions. Each kind of question, at each number of options,
  has a temperature that divides the logits before the softmax, so that an
  answer given 0.8 is right about 80% of the time. A temperature keeps each
  question's ranking, so a model's bias toward a label, such as `A`, stays as
  it was. The caller passes the temperatures, and a tool in `tools/` fits them
  from questions with known answers. Finished when `design.md` records, on
  questions held out of the fit, the expected calibration error before and
  after, and for each label how often it is the answer given and how often it
  is the right one. It names the data set and its license.
- [ ] A model larger than the guest's heap splits. `onnx_split` writes the
  checkpoint a tensor at a time rather than holding the model. Finished when
  Gemma 4 E2B's export splits and `loam dump` reports its decoder and its
  embedding graph, each refusal naming the operator it stops at.
- [ ] More elementwise operators. `Equal`, `Less`, `LessOrEqual`, `Greater`,
  `GreaterOrEqual`, `Not`, `And` and `Or`, as `Expr` steps that yield a
  `bool`, and `Softplus` and `Reciprocal`. Finished when `operators_test.wado`
  holds each to onnxruntime, and a mask built from a comparison joins the
  kernel of the `Where` reading it.
- [ ] The onnxruntime operators of a recurrent decoder: `LinearAttention`
  with each of its update rules and `CausalConvWithState`, and a
  `GroupQueryAttention` that rotates part of each head. Finished when
  `operators_test.wado` holds each to onnxruntime, with and without a past
  state, the cases that read a second output among them.
- [ ] onnxruntime's `RotaryEmbedding` and `FastGelu`. Finished when
  `operators_test.wado` holds each to onnxruntime, a rotation of part of each
  head by `rotary_embedding_dim` and one by position ids among the cases.
- [ ] Qwen 3.5 0.8B, a decoder whose layers are mostly recurrent. Finished
  when `example/qwen3.5-0.8b/` continues a prompt with the tokens onnxruntime
  picks, under `wado run-webgpu` from onnx-community's `q4` export of the
  text model, with no line that names a layer, and its tokenizer gives the
  ids Hugging Face's gives.
- [ ] A table stays where it is stored. A weight that only a `Gather` reads,
  as an embedding is, is prepared as a table whose storage the backend
  chooses, and a pass reads only the rows it gathers. Finished when Gemma 4
  E2B's per-layer embedding, which has more elements than a `List` holds,
  serves its rows with the host holding none of the rest.
- [ ] Gemma 4 E2B. Finished when `example/gemma4-e2b/` continues a prompt
  with the tokens onnxruntime picks, under `wado run-webgpu` from
  onnx-community's `q4` export, with no line that names a layer.
- [ ] The speed at which a weight reaches the device. `wado run-webgpu` moves
  a checkpoint's bytes from a file into a device buffer through
  `wasi:filesystem` and `write-buffer-with-copy`, and that speed sets what a
  weight the device does not hold costs. Finished when `design.md` records it
  on an RTX 4060 Ti of 8 GB as a ratio to a native program uploading the same
  bytes, with the file in the page cache and out of it, beside the commands
  that measured both.
- [ ] Experts loaded ahead. While one layer runs, the nodes that compute the
  next `QMoE`'s `router_probs` from its `input` are applied to this layer's
  `input`, and the experts they pick load before a token routes to them. A
  wrong guess costs a load, never a token. Finished when `design.md` records
  how often gpt-oss-20b's routed experts were already held with and without
  it, and its tokens are unchanged.
- [ ] The StableHLO front end. Finished when the MLP exported from JAX checks
  and runs through the same pipeline as its ONNX form.
- [ ] int4 through ONNX's own operators. `DequantizeLinear` with a
  `block_size` before the `MatMul` it feeds is a quantized product, as
  `MatMulNBits` is. Finished when
  an int4 checkpoint from GPTQ or AWQ produces the f16 checkpoint's output
  within tolerance.

## Known gaps

### Operators

`conformance/operators_test.wado` lists the operators Loam lowers. Four
outside ONNX's default domain are among them, onnxruntime's:

- `GroupQueryAttention`, without position ids, interleaved rotation or a
  quantized cache.
- `MatMulNBits`, without `g_idx`, a prepacked weight, or zero points in the
  input's float.
- `GatherBlockQuantized`, over a table of rank 2 gathered along its rows and
  quantized along its columns.
- `SkipSimplifiedLayerNormalization`, without the mean and the inverse
  deviation training reads.

ONNX's `Attention` is not. Any other operator stops the build naming itself.

A node other than `Split`, `If` and `GroupQueryAttention` produces one output.
`TopK` and the recurrent operators name several, and stage 0 gives only those
three a shape per output.

An operator reads only the attributes its shape rule, fold or kernel consults,
and is checked as if the rest were absent. A `Constant` or `ConstantOfShape`
whose `value` holds a type other than float, float16, bfloat16, int32, int64 or
bool stops the build. A `double` would narrow to f32, which is the element-type
gap under Element types and precision.

ONNX local functions. The textual format lets a model carry its own function
definitions and call them as operators, and the grammar covers neither, nor the
`@name` attribute reference only a function body can carry, so a model holding
one does not parse. What they admit is an operator set a model brings with it
rather than one Loam knows, which the shape algebra has no account of.

### Stage 0

Where no generic signature relates an operation's operands to its result, the
operation takes the axis lists as type parameters and trusts them, as `reshape`
does, and the generated call spells the lists stage 0 computed. A permutation of
three or more axes is one such operation, since each of the `n!` orders is its
own type. So is a product that drops a rank-1 operand's axis or stretches a batch
axis of 1, and an elementwise operand that stretches an axis of 1.

Stage 0 evaluates `Shape`, so one whose operand the build does not know stops
the build. `Slice`, `Range` and `ConstantOfShape` read their bounds or their
shape at build time, so one the run computes stops the build as well. So does a
`Range` whose delta has a sign the build cannot tell, since that decides which
way it counts.

Stage 0 bounds each term of a condition apart, takes a bound past 2^64 as none,
and stops comparing through derived symbols after a fixed number of failures. So
a condition that holds for every input may still be owed, and one false for
every input owed rather than refused.

A derived symbol is read as its operand only where two extents must be equal or
one must be 1, a `Reshape`'s element count and the sum of a `Split`'s parts
among them. Only the difference of the two extents is read, so a derived term
both share owes nothing.

An integer `Div` truncates toward zero, and stage 0 computes it over numbers.
Over a symbol, it stops the build unless the dividend is never negative and the
divisor is a positive number. There truncating rounds down, and a derived symbol
rounds down.

A value `compute` computes from the symbols, an extent or an owed slack, must
fit `i64` with every symbol as large as a length can be, or the build stops.
That bound takes each symbol apart from the others, so it also stops a length
the inputs bound together. `[N, M, 3]` flattened is `3 * N * M` elements, which
no input carrying `N` and `M` on one tensor takes past a list, and still stops
the build.

A `dim_param` spells arithmetic only in the whitespace-separated form The Graph
Is ONNX gives. `past_sequence_length+1` is one opaque name, and so is any
spelling with parentheses or division.

### Graphs and branches

A model exported as two graphs carries its weights twice. GPT-2's decoder and
its decoder with a past each name every weight, so each generated module's
`Weights` holds a copy, and a program running both holds two. The merged export
is one graph and holds them once.

A branch that declares inputs or initializers of its own stops the build. So
does an attribute holding a list of graphs.

An `If` output whose branches give it two lengths has a length nothing binds.
A node after the `If` that reads that length stops the build.

A condition the build knows still compiles both branches.

A join one branch makes holds in the other, since the branches share their
classes. A merged decoder's branch without a past joins its keys with its
queries, so in the branch with one a key and a query are one type, and so are
the mask and the input ids at the boundary.

An initializer the textual format gives a `["location": ...]` list stops the
build, naming the tensor. The protobuf form's `external_data` is read in place
instead, as `design.md` describes. A constant kept in a data file stops the
build, since the build reads a constant and Kiln hands the generator the model
alone.

### Element types and precision

`WebGpuBackend` computes in f32 between the f16s it reads and writes
([Half Element Types](./design.md#half-element-types)), so a device whose f16
arithmetic is faster gains nothing from it. No Kiln option chooses arithmetic
in f16 yet. One would, on a device that offers `shader-f16`, and a graph built
with it would be held to a tolerance rather than to the f32 path's bits.

Of its decode of Gemma 3's `q4` export
([benchmark](./benchmark/README.md#the-q4-and-q4f16-exports)), the quantized
products take most of the device time (`benchmark/gemma3_270m/profile.wado`).
A norm runs apart from the product it feeds. The `q4f16` export's products
are slower still: each weight rounds to f16 through an emulated conversion.

A tensor is held in `f32`, `f16`, `bf16`, `u8`, `i64` or `bool`, and every
other element type stops the build naming it. So does a `CastLike`
of a value computed at run, since it converts only a tensor the build knows.
Arithmetic in `double` and the integer types is a second instantiation of the
same kernels. Wado has a primitive for nearly every ONNX element type, so what
a step adds is kernels that compute in one, never the mapping.

A half graph computes at the precision its exporter chose, and a caller cannot
keep a node's output in f32 instead. An exporter that keeps a softmax or a norm
in f32 says so in the graph; one that does not leaves the rounding to Loam.

What the tolerance is is open: for one operator and for a whole model, and for
a half graph, where onnxruntime computes some operators in f32 and others in
half. So is how a decoder's token check treats two logits the tolerance cannot
tell apart.

Quantization beyond int4. Block formats are parameterized by block size, scale
representation, and rotation, so the type that carries one must be
parameterized the same way rather than named after a format; f8 belongs here,
since it is never shipped without its scales. GGUF as an input format arrives
with this, not before it, and its metadata is a binary key-value block rather
than JSON, so its probe has to parse one to report an extent.

### Conformance

The conformance suite reaches the graphs ONNX commits, not the per-operator
cases. `onnx/backend/test/data/node` is generated rather than checked in, so
what `conformance/` runs against is `pytorch-operator/` and
`pytorch-converted/`: whole small graphs with their inputs and outputs beside
them.

### WebGPU

The WebGPU backend runs `convolution` and `reduce_window` on the CPU, each
downloading its operand. So does a kernel over an element type other than a
float or `bool`, a product with more batch elements or tiles than one dispatch dimension holds,
and an elementwise kernel, a row kernel or a product whose output outgrows one
binding. A model whose graph alternates the two crosses the boundary at every
such node.

An operation over weights alone runs on every call to `compute`, where
onnxruntime folds it into the weight once. MNIST-12 regroups its last weight
with a `Reshape`.

A kernel reading more buffers than the adapter binds to one shader copies its
operands into one buffer first, so it moves them twice.

Merging attention heads after their transpose is a `reshape` no strides say, so
the device copies it: one dispatch a layer that a view cannot spare.

Products of one input are joined only where each writes its product alone, by
a weight. A projection with a bias after it, as some models give attention's
query, key and value, stays a product of its own. Over more than one row, a
part of a concatenation is strided by the whole product's row, so a `reshape`
merging its rows with an axis after them copies it, as Gemma 3's query and key
normalization over heads does on a prompt.

The weights of one `Joined` or `Paired` product are held on the host together
while the device's copy of them is laid out, since a half fits them only where
it holds every one's values. A load stays a tensor at a time elsewhere.

A buffer crosses the component boundary as a `list<u8>`, which the guest
converts from or to f32 one element at a time, so crossing costs more than the
device's own copy.

No Loam model has run on a phone, and no host runs a Wado guest with
`wasi:webgpu` on one.

Whether the backend should be an effect after all. The trait is chosen because
its cost is known to be zero and the effect's is unmeasured. A device builds the
graph once per shape class, so on one either cost falls only on a shape class's
first call.

### Devices

[Loam on TPU and WebGPU](./devices.md#known-gaps) lists the gaps of the device
design.

### Beyond tier 2

Tier 3, dynamic dispatch. Speculative decoding and early exit are
data-dependent and no bucketing covers them. Closing it needs the escalation
boundary drawn as a decidability question, as Gale's is, rather than as a
heuristic.

Training. Loam is an inference design. An autograd layer over the same shape
algebra is plausible and unowned.
