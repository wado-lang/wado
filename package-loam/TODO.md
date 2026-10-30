# Loam TODO

Open work on Loam. [`AGENTS.md`](./AGENTS.md) says how to work here, and
[`design.md`](./design.md) holds the design and names the documents it builds
on. A section name below, such as The Graph Is ONNX, is one of
[WEP: Loam](../docs/wep-2026-09-20-loam.md)'s.

The roadmap is ordered, and each item says what finishing it means. A known gap
says what is missing and what it admits, not how to close it. A finished item
leaves this file, and `design.md` states what it built.

## Roadmap

1. [ ] TPU and WebGPU at their best speed. A pass is a graph built once per
   shape class and compiled for each device: StableHLO through `wado:pjrt` for
   a TPU, Loam's own compiler for WebGPU. See
   [Loam on TPU and WebGPU](./devices.md), whose plan lists the steps, a TPU
   run and many requests at once among them. Finished when every step of that
   plan is.
2. [ ] Narrowed outputs. A use site can name an output that each call reads at
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
3. [ ] Decisions. A decision asks a model one question about a state and
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
4. [ ] A state read by many questions. The state's tokens run once. Each
   question runs only its own tokens, against the state's cache, and leaves
   that cache as it was. Finished when `design.md` records the time several
   questions take against one state of 1,000 tokens as a ratio to the time
   with the state run for each, beside the commands that measured both, and
   each probability lies within the tolerance of the one it gets with the
   state run for it.
5. [ ] Questions in one pass. The questions on one state run as the rows of
   one pass, so the device reads each weight once for all of them. A shorter
   question is padded at its end, and its answer is read at its own last
   token. Every row reads the state's cache, which keeps its batch of one
   rather than being copied for each row. The graph gives the past and the new
   tokens one batch axis, and `forward` checks that the inputs agree on it
   (see Axis Identity Is a Type, Extents Are Values). This item changes that
   check to admit a past of batch one beside n rows. Finished when `design.md`
   records the time n questions take as rows of one pass as a ratio to the
   time they take one after another, for several n and questions of different
   lengths, and each probability lies within the tolerance of the one its
   question gets alone.
6. [ ] Calibrated decisions. Each kind of question, at each number of options,
   has a temperature that divides the logits before the softmax, so that an
   answer given 0.8 is right about 80% of the time. A temperature keeps each
   question's ranking, so a model's bias toward a label, such as `A`, stays as
   it was. The caller passes the temperatures, and a tool in `tools/` fits them
   from questions with known answers. Finished when `design.md` records, on
   questions held out of the fit, the expected calibration error before and
   after, and for each label how often it is the answer given and how often it
   is the right one. It names the data set and its license.
7. [ ] A model larger than the guest's heap splits. `onnx_split` writes the
   checkpoint a tensor at a time rather than holding the model. Finished when
   Gemma 4 E2B's export splits and `loam dump` reports its decoder and its
   embedding graph, each refusal naming the operator it stops at.
8. [ ] More elementwise operators. `Equal`, `Less`, `LessOrEqual`, `Greater`,
   `GreaterOrEqual`, `Not`, `And` and `Or`, as `Expr` steps that yield a
   `bool`, and `Softplus` and `Reciprocal`. Finished when `operators_test.wado`
   holds each to onnxruntime, and a mask built from a comparison joins the
   kernel of the `Where` reading it.
9. [ ] Nodes of several outputs. Stage 0 gives every node a shape per output,
   as it does `Split`'s. Finished when the cases of a recurrent decoder's
   onnxruntime operators that read a second output pass.
10. [ ] The onnxruntime operators of a recurrent decoder: `LinearAttention`
    with each of its update rules, `CausalConvWithState`, and
    `SkipSimplifiedLayerNormalization`, and a `GroupQueryAttention` that
    rotates part of each head. Finished when `operators_test.wado` holds each
    to onnxruntime, with and without a past state.
11. [ ] onnxruntime's `RotaryEmbedding` and `FastGelu`. Finished when
    `operators_test.wado` holds each to onnxruntime, a rotation of part of each
    head by `rotary_embedding_dim` and one by position ids among the cases.
12. [ ] Loading a tensor at a time. `load` reads the checkpoint from a source
    that yields one tensor's bytes at a time, rather than from one `ByteList`
    the host holds whole. Finished when a checkpoint larger than the guest's
    heap loads onto `WebGpuBackend`.
13. [ ] onnxruntime's int4 operators, `MatMulNBits` and `GatherBlockQuantized`,
    which onnx-community's `q4` exports use. A weight stays packed on the
    device, read by the quantized product composite or by a quantized gather
    (see [Loam on TPU and WebGPU](./devices.md)).
    Finished when Gemma 3 (270M)'s `q4` export picks the tokens onnxruntime
    picks from it.
14. [ ] Qwen 3.5 0.8B, a decoder whose layers are mostly recurrent. Finished
    when `example/qwen3.5-0.8b/` continues a prompt with the tokens onnxruntime
    picks, under `wado run-webgpu` from onnx-community's `q4` export of the
    text model, with no line that names a layer, and its tokenizer gives the
    ids Hugging Face's gives.
15. [ ] A table stays where it is stored. A weight that only a `Gather` reads,
    as an embedding is, is prepared as a table whose storage the backend
    chooses, and a pass reads only the rows it gathers. Finished when Gemma 4
    E2B's per-layer embedding, which has more elements than a `List` holds,
    serves its rows with the host holding none of the rest.
16. [ ] Gemma 4 E2B. Finished when `example/gemma4-e2b/` continues a prompt
    with the tokens onnxruntime picks, under `wado run-webgpu` from
    onnx-community's `q4` export, with no line that names a layer.
17. [ ] A half graph keeps its type. Every kernel stores the element type the
    graph gives its output, `Prepared` holds a half weight as a half, and
    `WebGpuBackend` computes in f16 where the device offers `shader-f16` (see
    Wado Stores a Half, a Device Computes In It). Finished when Gemma 3
    (270M)'s `fp16` export picks the tokens onnxruntime picks from it on `Cpu`
    and on `WebGpuBackend` with and without `shader-f16`, and the deviation of
    each path's logits from onnxruntime's is recorded.
18. [ ] onnxruntime's `QMoE`, and a `GroupQueryAttention` with attention
    sinks. Finished when `operators_test.wado` holds each to onnxruntime.
19. [ ] The speed at which a weight reaches the device. `wado run-webgpu` moves
    a checkpoint's bytes from a file into a device buffer through
    `wasi:filesystem` and `write-buffer-with-copy`, and that speed sets what a
    weight the device does not hold costs. Finished when `design.md` records it
    on an RTX 4060 Ti of 8 GB as a ratio to a native program uploading the same
    bytes, with the file in the page cache and out of it, beside the commands
    that measured both.
20. [ ] Experts held on demand. An expert is one slice along the first axis of
    each `QMoE` input the operator's definition indexes by expert, so no model
    is named. The device holds as many experts as a budget the caller sets. The
    rest stay in the checkpoint until a token routes to one, and the guest's
    heap only passes an expert through. A table that stays where it is stored,
    above, serves its rows the same way. An expert the device does not hold is
    loaded, never computed by the guest. A guest runs on one thread, since
    shared-everything threads is at phase 1, and `core:simd` loads no vector
    from a `List`. Where the budget holds fewer than all the experts, the host
    reads back which experts a layer routes to before it runs them. So the host
    waits once a layer, which The MVP Is Tier 1 Only says a `QMoE` spares it.
    Finished when `operators_test.wado` holds a `QMoE` to onnxruntime with a
    budget of half its experts.
21. [ ] gpt-oss-20b, a mixture of experts. Finished when `example/gpt-oss-20b/`
    continues a prompt with the tokens onnxruntime picks from onnx-community's
    `q4f16` export, under `wado run-webgpu` on a device with 8 GB, with no line
    that names a layer, and `design.md` records how often a routed expert was
    already held, with the eviction order it chose and why.
22. [ ] Experts loaded ahead. While one layer runs, the nodes that compute the
    next `QMoE`'s `router_probs` from its `input` are applied to this layer's
    `input`, and the experts they pick load before a token routes to them. A
    wrong guess costs a load, never a token. Finished when `design.md` records
    how often gpt-oss-20b's routed experts were already held with and without
    it, and its tokens are unchanged.
23. [ ] The StableHLO front end. Finished when the MLP exported from JAX checks
    and runs through the same pipeline as its ONNX form.
24. [ ] int4. `DequantizeLinear` with a `block_size` fused into the `MatMul` it
    feeds, which is what makes a model small enough to download. Finished when
    an int4 checkpoint from GPTQ or AWQ produces the f16 checkpoint's output
    within tolerance.

## Known gaps

### Operators

The operator set is `MatMul`, the binary and unary elementwise operators,
`Transpose`, a `Gemm` lowered to those kernels rather than to one of its own,
`Reshape`, `Flatten`, `Unsqueeze` and `Squeeze`, `Split`, `Concat`, `MaxPool`
and `Conv` over any number of spatial axes, `Softmax`, `ReduceMean` and
`ReduceSum`, `Where`, `Gather`, `Slice`, `Range`, `ConstantOfShape`, `Cast`,
`Identity`, `Expand` and `If`. `Gelu` and onnxruntime's
`SimplifiedLayerNormalization` are read as the functions they are defined as,
over the operators above. One operator outside ONNX's default domain is
lowered, onnxruntime's `com.microsoft.GroupQueryAttention`, without position
ids, interleaved rotation or a quantized cache. Gemma 3 (270M), in
`example/gemma3-270m/`, needs all of these.

A node other than `Split`, `If` and `GroupQueryAttention` produces one output.
`TopK` and the recurrent operators name several, and stage 0 gives only those
three a shape per output.

An operator reads only the attributes its shape rule, fold or kernel consults,
and is checked as if the rest were absent. The readers carry every attribute as
the text it was written as, and whether it was written as strings, since a
string may read as `3`. An attribute of the wrong kind stops the build. A tensor
attribute is carried when its elements are float, float16, bfloat16, int32,
int64 or bool. A half is widened to f32, which holds each of its values exactly.
It keeps the type it is stored as, so the printer writes it back as bit
patterns. A `Constant` or `ConstantOfShape` whose `value` holds any other type
stops the build. A `double` would narrow to f32, which is the element-type gap
under Element types and precision.

ONNX local functions. The textual format lets a model carry its own function
definitions and call them as operators, and the grammar covers neither, nor the
`@name` attribute reference only a function body can carry, so a model holding
one does not parse. What they admit is an operator set a model brings with it
rather than one Loam knows, which the shape algebra has no account of.

### Stage 0

Where no generic signature relates a kernel's operands to its result, the kernel
takes the axis lists as type parameters and trusts them, as `reshape` does. The
generated call spells the lists stage 0 computed. A permutation of three or more
axes is one such kernel, `permute`, since each of the `n!` orders is its own
type. So is `matmul_broadcast`, which serves every product whose operands drop
a rank-1 operand's axis or stretch a batch axis of 1. So is `lift`, which reads
an elementwise operand under the axes it meets where it stretches an axis of 1,
which stays an axis of its own.

Stage 0 evaluates `Shape`, and no kernel computes it, so it stops the build
where the build does not know its operand. `Concat` has a kernel, and stage 0
evaluates one whose operands the build knows. `Slice`, `Range` and
`ConstantOfShape` have kernels, but read their bounds or their shape at build
time, so one the run computes stops the build as well. So does a `Range` whose
delta has a sign the build cannot tell, since that decides which way it counts.
Every other operator stops the build naming itself.

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

A value `forward` computes from the symbols, an extent or an owed slack, must
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
instead, as `design.md` describes, from one file. A model spreading its weights
over several data files stops the build naming two of them, and so does a
constant kept in one, since the build reads a constant and Kiln hands the
generator the model alone.

### Element types and precision

Every arithmetic kernel computes in f32 and stores f32, as the WEP's signatures
spell, but a half graph needs it to store `f16` or `bf16` as well. A
`WebGpuBackend` view holds f32 alone, and `forward` converts a half input and
output where it crosses the boundary. One of `int64` is held in `i64`, and one
of `bool` in `bool`. Every other element type stops the build naming it.
Arithmetic in `double` and the integer types is a second instantiation of the
same kernels. Wado has a primitive for nearly every ONNX element type, so what
a step adds is kernels that compute in one, never the mapping.

A half graph computes at the precision its exporter chose, and a caller cannot
keep a node's output in f32 instead. An exporter that keeps a softmax or a norm
in f32 says so in the graph; one that does not leaves the rounding to Loam.

`WebGpuBackend` takes the f16 path wherever the device offers `shader-f16`, and
a caller cannot take the f32 path on such a device.

What the tolerance is is open: for one operator and for a whole model, and for
a half graph, where onnxruntime computes some operators in f32 and others in
half. So is how a decoder's token check treats two logits the tolerance cannot
tell apart. `fusion_test.wado` still holds the fused module to the unfused one
bit for bit, which the kernels give today and Loam no longer promises.

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

The WebGPU backend runs the convolutions and `max_pool` on the CPU, each
downloading its operand. So does a kernel over an element type other than f32,
a product with more batch elements or tiles than one dispatch dimension holds,
and an elementwise kernel, a row kernel or a product whose output outgrows one
binding. A model whose graph alternates the two crosses the boundary at every
such node.

A kernel over weights alone runs on every call to `forward`. MNIST-12 regroups
its last weight with a `Reshape`, which `loam dump` reports as a kernel and
onnxruntime folds into the weight once.

A kernel reading more buffers than the adapter binds to one shader copies its
operands into one buffer first, so it moves them twice.

Merging attention heads after their transpose is a `reshape` no strides say, so
the device copies it: one dispatch a layer that a view cannot spare.

Products of one input are concatenated only where each is a kernel alone, by a
weight as stored. A projection with a bias after it, as some models give
attention's query, key and value, stays a product of its own, and so does one
by a transposed weight. Over more than one row, a part of a concatenation is
strided by the whole product's row, so a `reshape` merging its rows with an
axis after them copies it, as Gemma 3's query and key normalization over heads
does on a prompt.

A buffer of the pass is released when the next pass begins, never while this
one runs, so a pass holds every tensor it computes at once.

A shader may assume no NaN reaches it, as WGSL allows. Where the CPU keeps a
NaN, `Relu` among them, the device may give a number.

WGSL leaves a value past a half's range indeterminate, whether `pack2x16float`
stores it or f16 arithmetic computes it. So a half graph that reaches one, a
mask of negative infinity stored in a half among them, gets whatever the device
makes of it.

A buffer crosses the component boundary as a `list<u8>`, which the guest
converts from or to f32 one element at a time, so crossing costs more than the
device's own copy. Under `wado run-webgpu`, whose wasmtime is a generation
behind `wado run`'s, a loop allocating per element (`map` and `collect`) runs
twenty times slower than under `wado run`. The backend writes its own loops for
that reason.

Gemma 3 (270M) generates about 2% slower on `WebGpuBackend` than at
`73611c2bfc0`, where the `loam-cpu-perf` branch began, while it loads in half
the time. That is `mise run gemma3-270m` in `benchmark/`, best of three, with
both trees on one host; the three runs of each do not overlap. The cause is
not known.

No Loam model has run on a phone, and no host runs a Wado guest with
`wasi:webgpu` on one.

Every `WebGpuBackend` method spells the same ten `wasi:webgpu` resources, since
an impl method's `with _` resolves to nothing at a caller.

Whether the backend should be an effect after all. The trait is chosen because
its cost is known to be zero and the effect's is unmeasured. Once a device
builds the graph once per shape class ([Loam on TPU and WebGPU](./devices.md)),
either cost falls only on the first call of a shape class, which builds it.

### Beyond tier 1

Tier 3, dynamic dispatch. Speculative decoding and early exit are
data-dependent and no bucketing covers them. Closing it needs the escalation
boundary drawn as a decidability question, as Gale's is, rather than as a
heuristic.

Training. Loam is an inference design. An autograd layer over the same shape
algebra is plausible and unowned.
