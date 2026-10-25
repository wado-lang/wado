// Writes operators/<case>.onnx beside this script, one model per case of an
// operator Loam lowers, over static shapes and with the attributes and edge
// values where an implementation's reading is least safe. Beside each goes
// <case>.txt: the inputs the case fed, and what onnxruntime computed from
// them, which operators_test.wado holds Loam to.
//
//   mise run loam-oracle-deps
//   node operators.mjs <this directory>
import { mkdirSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import * as ort from 'onnxruntime-node';
import {
  BFLOAT16, BOOL, FLOAT, FLOAT16, INT32, INT64, f32, graphProto, modelProto, node, tensorProto, valueInfo,
} from '../tests/onnx_proto.mjs';

const here = process.argv[2] ?? dirname(fileURLToPath(import.meta.url));

// A graph input or initializer: its element type, its dims, and its elements,
// which a float input takes from `halves` where it names none.
const tensor = (type, dims, values) => ({ type, dims, values });
const float = (dims, values) => tensor(FLOAT, dims, values);
const int64 = (dims, values) => tensor(INT64, dims, values.map(BigInt));

// Multiples of 0.5 from -3 to 3, in an order no axis repeats: every float a
// case feeds is exact, 0 and the negatives among them.
const halves = (count) => Array.from({ length: count }, (_, i) => (((i * 7) % 13) - 6) / 2);

// Each case computes `Y`. `inputs` are fed and `inits` are initializers. A case
// whose opset onnxruntime does not run names in `oracle` the nodes,
// initializers and opset of a graph that computes the same, which onnxruntime
// runs instead.
const cases = [];
const add = (name, spec) => cases.push({ name, out: FLOAT, inits: {}, opset: 13, ...spec });

const pair = (a, b) => ({ A: float(a), B: float(b) });
const binary = (op, a, b) => ({ inputs: pair(a, b), nodes: [node(op, ['A', 'B'], 'Y')] });
add('AddSame', binary('Add', [2, 3], [2, 3]));
add('AddSuffix', binary('Add', [2, 3], [3]));
add('AddPrefix', binary('Add', [3], [2, 3]));
add('MulStretchRight', binary('Mul', [2, 3], [2, 1]));
add('SubStretchLeft', binary('Sub', [1, 3], [2, 3]));
add('AddStretchBoth', binary('Add', [2, 1], [1, 3]));
add('MulStretchLeftLast', binary('Mul', [2, 1], [2, 3]));
add('SubStretchRanks', binary('Sub', [3, 1, 4], [2, 4]));
add('DivByZero', {
  inputs: { A: float([2, 3], [1, -1, 0, 3, -3, 0.5]), B: float([2, 3], [0, 0, 0, 2, -0.5, 4]) },
  nodes: [node('Div', ['A', 'B'], 'Y')],
});
add('PowTensor', binary('Pow', [2, 3], [2, 3]));
add('PowHalf', {
  inputs: { A: float([2, 3]) },
  inits: { E: float([], [0.5]) },
  nodes: [node('Pow', ['A', 'E'], 'Y')],
});
add('SubFromScalar', {
  inputs: { A: float([2, 3]) },
  inits: { E: float([], [1.5]) },
  nodes: [node('Sub', ['E', 'A'], 'Y')],
});

for (const op of ['Relu', 'Sigmoid', 'Tanh', 'Erf', 'Exp', 'Log', 'Sqrt', 'Neg', 'Abs', 'Identity']) {
  add(op, { inputs: { A: float([2, 3]) }, nodes: [node(op, ['A'], 'Y')] });
}

add('MatMul', binary('MatMul', [2, 3], [3, 4]));
add('MatMulBatched', binary('MatMul', [2, 2, 3], [2, 3, 4]));
add('MatMulShared', binary('MatMul', [2, 2, 3], [3, 4]));
add('MatMulRowVector', binary('MatMul', [3], [3, 4]));
add('MatMulColumnVector', binary('MatMul', [2, 3], [3]));
add('MatMulBroadcastBatch', binary('MatMul', [1, 2, 3], [2, 3, 4]));
// A language model's head: the product by a weight's transpose, which Loam
// reads as the weight is stored, with an operator after it to join.
add('MatMulTransposedWeight', {
  inputs: { A: float([2, 2, 5]) },
  inits: { E: float([6, 5], halves(30).map((v) => v - 0.3)) },
  nodes: [node('Transpose', ['E'], 'T'), node('MatMul', ['A', 'T'], 'P'), node('Relu', ['P'], 'Y')],
});

add('Gemm', {
  inputs: { A: float([2, 3]), B: float([3, 4]), C: float([4]) },
  nodes: [node('Gemm', ['A', 'B', 'C'], 'Y')],
});
add('GemmTransposedScaled', {
  inputs: { A: float([3, 2]), B: float([4, 3]), C: float([1, 4]) },
  nodes: [node('Gemm', ['A', 'B', 'C'], 'Y', { alpha: f32(0.5), beta: f32(2), transA: 1, transB: 1 })],
});
add('GemmScalarBias', {
  inputs: { A: float([2, 3]), B: float([3, 4]), C: float([]) },
  nodes: [node('Gemm', ['A', 'B', 'C'], 'Y')],
});

add('TransposeReversed', { inputs: { A: float([2, 3, 4]) }, nodes: [node('Transpose', ['A'], 'Y')] });
add('TransposeSwap', { inputs: { A: float([2, 3, 4]) }, nodes: [node('Transpose', ['A'], 'Y', { perm: [1, 0, 2] })] });
add('TransposeCycle', { inputs: { A: float([2, 3, 4]) }, nodes: [node('Transpose', ['A'], 'Y', { perm: [2, 0, 1] })] });

add('ReshapeKeepRest', {
  inputs: { A: float([2, 3, 4]) },
  inits: { S: int64([2], [0, -1]) },
  nodes: [node('Reshape', ['A', 'S'], 'Y')],
});
add('ReshapeSplit', {
  inputs: { A: float([2, 12]) },
  inits: { S: int64([3], [2, 3, 4]) },
  nodes: [node('Reshape', ['A', 'S'], 'Y')],
});
add('FlattenAll', { inputs: { A: float([2, 3, 4]) }, nodes: [node('Flatten', ['A'], 'Y', { axis: 0 })] });
add('FlattenLast', { inputs: { A: float([2, 3, 4]) }, nodes: [node('Flatten', ['A'], 'Y', { axis: -1 })] });
add('UnsqueezeEnds', {
  inputs: { A: float([2, 3]) },
  inits: { X: int64([2], [-1, 0]) },
  nodes: [node('Unsqueeze', ['A', 'X'], 'Y')],
});
add('SqueezeAll', {
  inputs: { A: float([1, 3, 1]) },
  nodes: [node('Squeeze', ['A'], 'Y')],
});
add('SqueezeLast', {
  inputs: { A: float([1, 3, 1]) },
  inits: { X: int64([1], [-1]) },
  nodes: [node('Squeeze', ['A', 'X'], 'Y')],
});

add('SplitEqual', {
  inputs: { A: float([2, 6]) },
  nodes: [node('Split', ['A'], ['Y', 'P', 'Q'], { axis: 1 })],
});
add('SplitSizes', {
  inputs: { A: float([2, 7]) },
  inits: { S: int64([2], [2, 5]) },
  nodes: [node('Split', ['A', 'S'], ['P', 'Y'], { axis: -1 })],
});

const image = (h, w) => ({ A: float([1, 2, h, w]) });
const pool = (h, w, attrs) => ({ inputs: image(h, w), nodes: [node('MaxPool', ['A'], 'Y', attrs)] });
add('MaxPool', pool(4, 4, { kernel_shape: [2, 2], strides: [2, 2] }));
add('MaxPoolPadded', pool(4, 4, { kernel_shape: [3, 3], pads: [1, 1, 1, 1] }));
add('MaxPoolCeil', pool(5, 5, { kernel_shape: [2, 2], strides: [2, 2], ceil_mode: 1 }));
add('MaxPoolSameUpper', pool(5, 5, { kernel_shape: [3, 3], strides: [2, 2], auto_pad: 'SAME_UPPER' }));
add('MaxPoolSameLower', pool(4, 4, { kernel_shape: [2, 2], auto_pad: 'SAME_LOWER' }));
add('MaxPoolDilated', pool(5, 5, { kernel_shape: [2, 2], dilations: [2, 2] }));
add('MaxPool1D', {
  inputs: { A: float([1, 2, 7]) },
  nodes: [node('MaxPool', ['A'], 'Y', { kernel_shape: [3], strides: [2] })],
});

add('Conv', {
  inputs: { A: float([1, 2, 4, 4]), W: float([3, 2, 3, 3]), B: float([3]) },
  nodes: [node('Conv', ['A', 'W', 'B'], 'Y', { pads: [1, 1, 1, 1] })],
});
add('ConvStridedDilated', {
  inputs: { A: float([1, 2, 7, 7]), W: float([3, 2, 2, 2]) },
  nodes: [node('Conv', ['A', 'W'], 'Y', { strides: [2, 2], dilations: [2, 2] })],
});
add('ConvSameLower', {
  inputs: { A: float([1, 2, 4, 4]), W: float([3, 2, 2, 2]) },
  nodes: [node('Conv', ['A', 'W'], 'Y', { auto_pad: 'SAME_LOWER' })],
});
add('Conv1D', {
  inputs: { A: float([1, 1, 6]), W: float([2, 1, 3]) },
  nodes: [node('Conv', ['A', 'W'], 'Y')],
});
add('ConvGrouped', {
  inputs: { A: float([1, 4, 3, 3]), W: float([2, 2, 2, 2]) },
  nodes: [node('Conv', ['A', 'W'], 'Y', { group: 2 })],
});
add('ConvDepthwise', {
  inputs: { A: float([1, 3, 4, 4]), W: float([3, 1, 3, 3]), B: float([3]) },
  nodes: [node('Conv', ['A', 'W', 'B'], 'Y', { group: 3, pads: [1, 1, 1, 1] })],
});

add('SoftmaxLast', { inputs: { A: float([2, 3]) }, nodes: [node('Softmax', ['A'], 'Y')] });
add('SoftmaxFirst', { inputs: { A: float([2, 3]) }, nodes: [node('Softmax', ['A'], 'Y', { axis: 0 })] });
// Before opset 7, arithmetic with `broadcast` aligned B at A's `axis`, which a
// B unsqueezed along the axes after it restates.
add('AddAtAxis', {
  inputs: pair([2, 3, 4], [3]),
  nodes: [node('Add', ['A', 'B'], 'Y', { broadcast: 1, axis: 1 })],
  opset: 6,
  oracle: {
    inits: { S: int64([1], [1]) },
    nodes: [node('Unsqueeze', ['B', 'S'], 'U'), node('Add', ['A', 'U'], 'Y')],
    opset: 13,
  },
});
add('SubAtLeadingAxes', {
  inputs: pair([2, 3, 4], [2, 3]),
  nodes: [node('Sub', ['A', 'B'], 'Y', { broadcast: 1, axis: 0 })],
  opset: 6,
  oracle: {
    inits: { S: int64([1], [2]) },
    nodes: [node('Unsqueeze', ['B', 'S'], 'U'), node('Sub', ['A', 'U'], 'Y')],
    opset: 13,
  },
});

// Before opset 13, Softmax normalized every axis from `axis` on as one lane.
add('SoftmaxFlattenedDefault', { inputs: { A: float([2, 3, 4]) }, nodes: [node('Softmax', ['A'], 'Y')], opset: 11 });
add('SoftmaxFlattenedFirst', {
  inputs: { A: float([2, 3, 4]) },
  nodes: [node('Softmax', ['A'], 'Y', { axis: 0 })],
  opset: 11,
});
add('SoftmaxMiddle', { inputs: { A: float([2, 3, 4]) }, nodes: [node('Softmax', ['A'], 'Y', { axis: 1 })] });
add('SoftmaxLarge', {
  inputs: { A: float([2, 3], [1000, 1001, 1002, -1000, 0, 1000]) },
  nodes: [node('Softmax', ['A'], 'Y')],
});

add('ReduceMeanKept', { inputs: { A: float([2, 3, 4]) }, nodes: [node('ReduceMean', ['A'], 'Y', { axes: [1] })] });
add('ReduceMeanDropped', {
  inputs: { A: float([2, 3, 4]) },
  nodes: [node('ReduceMean', ['A'], 'Y', { axes: [-1], keepdims: 0 })],
});
add('ReduceMeanAll', { inputs: { A: float([2, 3, 4]) }, nodes: [node('ReduceMean', ['A'], 'Y')] });
add('ReduceSumLast', {
  inputs: { A: float([2, 3, 4]) },
  inits: { X: int64([1], [-1]) },
  nodes: [node('ReduceSum', ['A', 'X'], 'Y')],
});
add('ReduceSumNoop', {
  inputs: { A: float([2, 3]) },
  nodes: [node('ReduceSum', ['A'], 'Y', { noop_with_empty_axes: 1 })],
});

// Loam holds no bool at the boundary, so the condition is cast from a float.
add('WhereBroadcast', {
  inputs: { C: float([2, 3], [1, 0, -2, 0, 0, 0.5]), A: float([2, 3]), B: float([3]) },
  nodes: [node('Cast', ['C'], 'M', { to: BOOL }), node('Where', ['M', 'A', 'B'], 'Y')],
});

add('GatherRows', {
  inputs: { A: float([4, 3]), I: int64([2], [3, -1]) },
  nodes: [node('Gather', ['A', 'I'], 'Y')],
});
add('GatherScalarIndex', {
  inputs: { A: float([4, 3]) },
  inits: { I: int64([], [1]) },
  nodes: [node('Gather', ['A', 'I'], 'Y')],
});
add('GatherMatrixIndices', {
  inputs: { A: float([4, 3]), I: int64([2, 2], [0, 1, 3, 2]) },
  nodes: [node('Gather', ['A', 'I'], 'Y')],
});
add('GatherColumns', {
  inputs: { A: float([4, 3]), I: int64([2], [2, 0]) },
  nodes: [node('Gather', ['A', 'I'], 'Y', { axis: 1 })],
});

add('SliceBackByTwo', {
  inputs: { A: float([4, 5]) },
  inits: { B: int64([1], [-1]), E: int64([1], [-10000]), X: int64([1], [-1]), P: int64([1], [-2]) },
  nodes: [node('Slice', ['A', 'B', 'E', 'X', 'P'], 'Y')],
});
add('SliceTwoAxes', {
  inputs: { A: float([4, 5]) },
  inits: { B: int64([2], [1, 0]), E: int64([2], [3, 5]), X: int64([2], [0, 1]), P: int64([2], [1, 2]) },
  nodes: [node('Slice', ['A', 'B', 'E', 'X', 'P'], 'Y')],
});

// A bool or an int32 is cast back to what the boundary holds.
const cast = (to, back, values) => ({
  inputs: { A: float([2, 3], values) },
  nodes: back === undefined
    ? [node('Cast', ['A'], 'Y', { to })]
    : [node('Cast', ['A'], 'M', { to }), node('Cast', ['M'], 'Y', { to: back })],
  out: back ?? to,
});
add('CastToInt64', cast(INT64));
add('CastToBool', cast(BOOL, FLOAT, [0, -0, 0.5, -3.5, Infinity, NaN]));
add('CastBoolToInt64', cast(BOOL, INT64, [0, -0, 0.5, -3.5, Infinity, NaN]));
add('CastToInt32', cast(INT32, INT64));
add('CastFromInt64', {
  inputs: { A: int64([2, 3], [-3, -1, 0, 1, 2, 9007199254740993]) },
  nodes: [node('Cast', ['A'], 'Y', { to: FLOAT })],
});

add('ConstantOfShape', {
  inputs: { A: float([2, 3]) },
  nodes: [
    node('Shape', ['A'], 'S'),
    node('ConstantOfShape', ['S'], 'Y', { value: { tensor: tensorProto('v', FLOAT, [1], new Float32Array([2.5]).buffer) } }),
  ],
});
// A half constant by its bit patterns: 1.5, -2, the half nearest 0.1, and the
// smallest subnormal.
const halfConstant = (type, bits) => ({
  inputs: { A: float([2, 4]) },
  nodes: [
    node('Constant', [], 'C', { value: { tensor: tensorProto('c', type, [4], new Uint16Array(bits).buffer) } }),
    node('Cast', ['C'], 'D', { to: FLOAT }),
    node('Add', ['A', 'D'], 'Y'),
  ],
});
add('ConstantFloat16', halfConstant(FLOAT16, [0x3e00, 0xc000, 0x2e66, 0x0001]));
add('ConstantBFloat16', halfConstant(BFLOAT16, [0x3fc0, 0xc000, 0x3dcd, 0x0001]));
add('ShapeWindow', {
  inputs: { A: float([2, 3, 4]) },
  nodes: [node('Shape', ['A'], 'Y', { start: 1, end: -1 })],
  out: INT64,
  opset: 15,
});
add('ConcatShapes', {
  inputs: { A: float([2, 3]), B: float([4]) },
  nodes: [node('Shape', ['A'], 'S'), node('Shape', ['B'], 'T'), node('Concat', ['S', 'T'], 'Y', { axis: 0 })],
  out: INT64,
});

// The operators ONNX defines as functions of others, and one onnxruntime adds to
// the default domain: an RMS normalization.
add('SimplifiedLayerNormalization', {
  inputs: { A: float([2, 4]) },
  inits: { S: float([4], [1, -0.5, 2, 0.25]) },
  nodes: [node('SimplifiedLayerNormalization', ['A', 'S'], 'Y', { epsilon: f32(1e-5), axis: -1 })],
  opset: 17,
});
add('GeluTanh', {
  inputs: { A: float([2, 3]) },
  nodes: [node('Gelu', ['A'], 'Y', { approximate: 'tanh' })],
  opset: 20,
});
add('GeluErf', { inputs: { A: float([2, 3]) }, nodes: [node('Gelu', ['A'], 'Y')], opset: 20 });

const expand = (dims, shape) => ({
  inputs: { A: float(dims) },
  inits: { S: int64([shape.length], shape) },
  nodes: [node('Expand', ['A', 'S'], 'Y')],
});
add('ExpandUnit', expand([2, 1], [2, 3]));
add('ExpandRank', expand([3], [2, 1, 3]));
// The target's 1 keeps the operand's extent, since Expand broadcasts both ways.
add('ExpandBothWays', expand([2, 1], [1, 3]));

// GroupQueryAttention over `s` new positions after `past` cached ones, `heads`
// query heads sharing `kv` key and value heads of `dim` elements. `rotary`
// feeds cos and sin caches of 8 positions; `bias` an additive mask; `output`
// names which of the three results the case checks. onnxruntime rotates a head
// of a multiple of 16 elements only.
const gqa = ({ s, past, heads, kv, dim, attrs = {}, rotary = false, bias = null, output = 0 }) => {
  const total = past + s;
  const cache = (f) => {
    const half = dim / 2;
    const values = [];
    for (let p = 0; p < 8; p++) {
      for (let i = 0; i < half; i++) values.push(f(p / 10 ** (i / half)));
    }
    return float([8, half], values);
  };
  const inputs = {
    Q: float([1, s, heads * dim]),
    K: float([1, s, kv * dim]),
    V: float([1, s, kv * dim], halves(s * kv * dim).map((v) => -v)),
    PK: float([1, kv, past, dim]),
    PV: float([1, kv, past, dim]),
  };
  const inits = {
    L: tensor(INT32, [1], [total - 1]),
    T: tensor(INT32, [], [total]),
  };
  const operands = ['Q', 'K', 'V', 'PK', 'PV', 'L', 'T'];
  if (rotary) {
    inits.C = cache(Math.cos);
    inits.N = cache(Math.sin);
    operands.push('C', 'N');
  }
  if (bias) {
    inputs.B = float([1, 1, s, total], bias);
    operands.push(...(rotary ? [] : ['', '']), '', 'B');
  }
  const outputs = ['O', 'PKo', 'PVo'];
  outputs[output] = 'Y';
  return {
    inputs,
    inits,
    nodes: [node('com.microsoft.GroupQueryAttention', operands, outputs, { num_heads: heads, kv_num_heads: kv, ...attrs })],
    opset: 17,
    domains: { 'com.microsoft': 1 },
  };
};
add('GqaPrompt', gqa({ s: 3, past: 0, heads: 2, kv: 1, dim: 16 }));
add('GqaPast', gqa({ s: 1, past: 3, heads: 4, kv: 2, dim: 16 }));
add('GqaPastPrompt', gqa({ s: 2, past: 3, heads: 2, kv: 2, dim: 16 }));
add('GqaRotary', gqa({ s: 3, past: 2, heads: 2, kv: 1, dim: 16, rotary: true, attrs: { do_rotary: 1 } }));
add('GqaRotaryPresentKey', gqa({ s: 2, past: 1, heads: 2, kv: 1, dim: 16, rotary: true, attrs: { do_rotary: 1 }, output: 1 }));
add('GqaPresentValue', gqa({ s: 2, past: 2, heads: 2, kv: 1, dim: 16, output: 2 }));
add('GqaWindow', gqa({ s: 5, past: 0, heads: 2, kv: 1, dim: 16, attrs: { local_window_size: 2 } }));
add('GqaWindowPast', gqa({ s: 2, past: 4, heads: 2, kv: 1, dim: 16, attrs: { local_window_size: 3 } }));
add('GqaScaleSoftcap', gqa({ s: 3, past: 1, heads: 2, kv: 1, dim: 16, attrs: { scale: f32(0.75), softcap: f32(1.5) } }));
add('GqaBias', gqa({ s: 2, past: 1, heads: 2, kv: 1, dim: 16, bias: [0, -3.4028234663852886e38, 0.5, 0, 0, -1] }));
add('GqaRotaryBias', gqa({
  s: 2, past: 1, heads: 2, kv: 1, dim: 16, rotary: true, attrs: { do_rotary: 1 }, bias: [0, 0, -2, -3.4028234663852886e38, 0, 0],
}));

const elementName = { [FLOAT]: 'f32', [INT64]: 'i64' };

// The tensor's elements as the .txt spells them: a float by its bits, so that
// the case reads back exactly what was fed and computed.
function spelled(type, data) {
  if (type === FLOAT) return Array.from(new Uint32Array(Float32Array.from(data).buffer));
  return Array.from(data, (v) => v.toString());
}

const line = (fields) => `${fields.join(' ')}\n`;
const list = (values) => `[${values.join(',')}]`;

const raw = (type, values) => {
  if (type === FLOAT) return new Float32Array(values).buffer;
  if (type === INT32) return new Int32Array(values).buffer;
  return new BigInt64Array(values).buffer;
};

const feed = (type, dims, values) => type === FLOAT
  ? new ort.Tensor('float32', Float32Array.from(values), dims)
  : new ort.Tensor('int64', BigInt64Array.from(values), dims);

function model({ name, inputs, inits, nodes, out, opset, domains }) {
  const graph = graphProto({
    name,
    nodes,
    initializers: Object.entries(inits).map(([i, t]) => tensorProto(i, t.type, t.dims, raw(t.type, t.values))),
    inputs: Object.entries(inputs).map(([i, t]) => valueInfo(i, t.type, t.dims)),
    outputs: [valueInfo('Y', out, null)],
  });
  return modelProto(graph, opset, domains);
}

mkdirSync(join(here, 'operators'), { recursive: true });
for (const c of cases) {
  const path = join(here, 'operators', `${c.name}.onnx`);
  writeFileSync(path, model(c));
  const lines = [];
  const feeds = {};
  for (const [i, t] of Object.entries(c.inputs)) {
    const count = t.dims.reduce((a, b) => a * b, 1);
    const values = t.values ?? halves(count);
    feeds[i] = feed(t.type, t.dims, values);
    lines.push(line(['input', i, elementName[t.type], list(t.dims), list(spelled(t.type, values))]));
  }
  const session = await ort.InferenceSession.create(c.oracle ? model({ ...c, ...c.oracle }) : path);
  const y = (await session.run(feeds, ['Y'])).Y;
  lines.push(line(['output', elementName[c.out], list(y.dims), list(spelled(c.out, y.data))]));
  writeFileSync(join(here, 'operators', `${c.name}.txt`), lines.join(''));
}
