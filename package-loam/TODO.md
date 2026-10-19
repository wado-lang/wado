# Loam TODO

Open work on Loam. [`AGENTS.md`](./AGENTS.md) says how to work here, and
[`design.md`](./design.md) holds the design and names the documents it builds
on. A section name below, such as A Decoder's Cache Is One Value, is one of
[WEP: Loam](../docs/wep-2026-09-20-loam.md)'s.

The roadmap is ordered, and each item says what finishing it means. A known gap
says what is missing and what it admits, not how to close it. A finished item
leaves this file, and `design.md` states what it built.

## Roadmap

1. [ ] The probe. Loam reports the header as a checkpoint's extent, so the
   build keys on the header rather than hashing and reading the whole file
   (see The Graph Is the Schema). Closing it takes the Kiln work its WEP
   records. Finished when a build naming GPT-2 (124M)'s whole checkpoint reads
   its header and none of its payloads, so the example no longer needs a
   header file of its own.
2. [ ] Sharded checkpoints. A model published in shards, with the index that
   names each tensor's file, arrives as several `inputs` entries. Finished
   when a sharded checkpoint checks and loads as its single-file form does,
   and a tensor the index places in a shard that is absent names the shard.
3. [ ] Inferred axis names. Loam names each axis from the graph's meetings, as
   Axis Names Are Inferred From the Graph says, and the `architecture` option
   is withdrawn. Finished when MNIST-12, the tiny GPT-2's three graphs, GPT-2
   (124M) and Gemma 3 (270M) build with no `layout` option, and `loam dump`
   reports where each axis took its name from.
4. [ ] The decoder's cache as one value. See A Decoder's Cache Is One Value.
   Finished when the GPT-2 and Gemma 3 examples pass a `Cache` and no line of
   theirs names a layer.
5. [ ] A model larger than the guest's heap splits. `onnx_split` writes the
   checkpoint a tensor at a time rather than holding the model. Finished when
   Gemma 4 E2B's export splits and `loam dump` reports its decoder and its
   embedding graph, each refusal naming the operator it stops at.
6. [ ] More elementwise operators. `Equal`, `Less`, `LessOrEqual`, `Greater`,
   `GreaterOrEqual`, `Not`, `And` and `Or`, as `Expr` steps that yield a
   `bool`, and `Softplus` and `Reciprocal`. Finished when `operators_test.wado`
   holds each to onnxruntime, and a mask built from a comparison joins the
   kernel of the `Where` reading it.
7. [ ] Nodes of several outputs. Stage 0 gives every node a shape per output,
   as it does `Split`'s. Finished when the cases of a recurrent decoder's
   onnxruntime operators that read a second output pass.
8. [ ] The onnxruntime operators of a recurrent decoder: `LinearAttention`
   with each of its update rules, `CausalConvWithState`, and
   `SkipSimplifiedLayerNormalization`, and a `GroupQueryAttention` that
   rotates part of each head. Finished when `operators_test.wado` holds each
   to onnxruntime, with and without a past state.
9. [ ] onnxruntime's `RotaryEmbedding` and `FastGelu`. Finished when
   `operators_test.wado` holds each to onnxruntime, a rotation of part of each
   head by `rotary_embedding_dim` and one by position ids among the cases.
10. [ ] Loading a tensor at a time. `load` reads the checkpoint from a source
    that yields one tensor's bytes at a time, rather than from one `ByteList`
    the host holds whole. Finished when a checkpoint larger than the guest's
    heap loads onto `WebGpuBackend`.
11. [ ] onnxruntime's int4 operators, `MatMulNBits` and `GatherBlockQuantized`,
    which onnx-community's `q4` exports use. A weight stays packed in the
    backend, as `Bk::Rhs` or as the packed tensor a quantized gather reads.
    Finished when Gemma 3 (270M)'s `q4` export picks the tokens onnxruntime
    picks from it.
12. [ ] Qwen 3.5 0.8B, a decoder whose layers are mostly recurrent. Finished
    when `example/qwen3.5-0.8b/` continues a prompt with the tokens onnxruntime
    picks, under `wado run-webgpu` from onnx-community's `q4` export of the
    text model, with no `layout` option and no line that names a layer, and
    its tokenizer gives the ids Hugging Face's gives.
13. [ ] A table stays where it is stored. A weight that only a `Gather` reads,
    as an embedding is, is prepared as a table whose storage the backend
    chooses, and a pass reads only the rows it gathers. Finished when Gemma 4
    E2B's per-layer embedding, which has more elements than a `List` holds,
    serves its rows with the host holding none of the rest.
14. [ ] Gemma 4 E2B. Finished when `example/gemma4-e2b/` continues a prompt
    with the tokens onnxruntime picks, under `wado run-webgpu` from
    onnx-community's `q4` export, with no `layout` option and no line that
    names a layer.
15. [ ] A half graph keeps its type. Every kernel stores the element type the
    graph gives its output, `Prepared` holds a half weight as a half, and
    `WebGpuBackend` computes in f16 where the device offers `shader-f16` (see
    Wado Stores a Half, a Device Computes In It). Finished when Gemma 3
    (270M)'s `fp16` export picks the tokens onnxruntime picks from it on `Cpu`
    and on `WebGpuBackend` with and without `shader-f16`, and the deviation of
    each path's logits from onnxruntime's is recorded.
16. [ ] onnxruntime's `QMoE`, and a `GroupQueryAttention` with attention
    sinks. Finished when `operators_test.wado` holds each to onnxruntime.
17. [ ] The speed at which a weight reaches the device. `wado run-webgpu` moves
    a checkpoint's bytes from a file into a device buffer through
    `wasi:filesystem` and `write-buffer-with-copy`, and that speed sets what a
    weight the device does not hold costs. Finished when `design.md` records it
    on an RTX 4060 Ti of 8 GB as a ratio to a native program uploading the same
    bytes, with the file in the page cache and out of it, beside the commands
    that measured both.
18. [ ] Experts held on demand. An expert is one slice along the first axis of
    each `QMoE` input the operator's definition indexes by expert, so no model
    is named. The device holds as many experts as a budget the caller sets. The
    rest stay in the checkpoint until a token routes to one, and the guest's
    heap only passes an expert through. An expert is held as the item that
    keeps a table where it is stored holds a row, with the device as the place
    it is held. An expert the device does not hold is loaded, never computed by
    the guest: a guest runs on one thread, since shared-everything threads is at
    phase 1, and `core:simd` loads no vector from a `List`. Where the budget
    holds fewer than all the experts, the host reads back which experts a layer
    routes to before it runs them. So the host waits once a layer, which The
    MVP Is Tier 1 Only says a `QMoE` spares it. Finished when
    `operators_test.wado` holds a `QMoE` to onnxruntime with a budget of half
    its experts.
19. [ ] gpt-oss-20b, a mixture of experts. Finished when `example/gpt-oss-20b/`
    continues a prompt with the tokens onnxruntime picks from onnx-community's
    `q4f16` export, under `wado run-webgpu` on a device with 8 GB, with no
    `layout` option and no line that names a layer, and `design.md` records how
    often a routed expert was already held, with the eviction order it chose and
    why.
20. [ ] Experts loaded ahead. While one layer runs, the nodes that compute the
    next `QMoE`'s `router_probs` from its `input` are applied to this layer's
    `input`, and the experts they pick load before a token routes to them. A
    wrong guess costs a load, never a token. Finished when `design.md` records
    how often gpt-oss-20b's routed experts were already held with and without
    it, and its tokens are unchanged.
21. [ ] The StableHLO front end. Finished when the MLP exported from JAX checks
    and runs through the same pipeline as its ONNX form.
22. [ ] int4. `DequantizeLinear` with a `block_size` fused into the `MatMul` it
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
an elementwise operand under the axes it meets where the layout names an axis
of 1 apart from the one it stretches to.

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

### Axis names

`layout` can name a merged axis by its parts, as `"(C H W)"`. That gives the
axis an extent but not its parts, so regrouping such an axis stops the build
naming the part.

How an axis name Loam mints is spelled is open, and so is which `dim_param` an
axis takes where several reach it. Each decides what a caller writes for an axis
no `layout` names. What one name means where it reaches two axes the graph keeps
apart, a `layout` word or a `dim_param`, is open too.

### Graphs and branches

A model exported as two graphs carries its weights twice. GPT-2's decoder and
its decoder with a past each name every weight, so each generated module's
`Weights` holds a copy, and a program running both holds two. The merged export
is one graph and holds them once.

A branch that declares inputs or initializers of its own stops the build. So
does an attribute holding a list of graphs.

An `If` output whose branches give it two lengths has a length nothing binds.
A node after the `If` that reads that length stops the build.

A constant that only branch-scoped layout rules name is spelled in each branch
that reads it.

A condition the build knows still compiles both branches.

An initializer naming its own data file stops the build, naming the tensor. The
textual format lets one carry a `["location": ...]` list, the protobuf form an
`external_data` entry, and Loam takes its weights from the checkpoint the
invocation names, so the two say different things about where one tensor's
bytes are. `tools/onnx_split.wado` reads the protobuf form's files from beside
the model, so a model kept that way is split into a checkpoint first.

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

No Loam model has run on a phone, and no host runs a Wado guest with
`wasi:webgpu` on one.

Every `WebGpuBackend` method spells the same ten `wasi:webgpu` resources, since
an impl method's `with _` resolves to nothing at a caller.

Whether the backend should be an effect after all. The trait is chosen because
its cost is known to be zero and the effect's is unmeasured. A second backend
makes the measurement possible.

### Beyond tier 1

Tier 2, the bucketed JIT. Residual sequence and batch extents need a
shape-class cache populated on first use. Closing it needs a cache key design, a
warmup story, and an answer to generation latency becoming user-visible. At
build time no user waits on the generator; at first use one does.

Tier 3, dynamic dispatch. Speculative decoding and early exit are
data-dependent and no bucketing covers them. Closing it needs the escalation
boundary drawn as a decidability question, as Gale's is, rather than as a
heuristic.

Training. Loam is an inference design. An autograd layer over the same shape
algebra is plausible and unowned.
