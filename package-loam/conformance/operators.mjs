// Writes operators/<case>.onnx beside this script, one model per case of an
// operator Loam lowers, over static shapes and with the attributes and edge
// values where an implementation's reading is least safe. Beside each goes
// <case>.txt: the inputs the case fed, and what onnxruntime computed from
// them, which operators_test.wado holds Loam to. A case onnxruntime departs
// from the operator's definition on names in `exact` a function computing the
// definition, which writes its output instead.
//
//   mise run loam-oracle-deps
//   node operators.mjs <this directory>
import { mkdirSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import * as ort from 'onnxruntime-node';
import {
  BFLOAT16, BOOL, FLOAT, FLOAT16, INT32, INT64, UINT8, f32, graphProto, modelProto, node, tensorProto, valueInfo,
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
// onnxruntime does not run as written names in `oracle` the nodes,
// initializers, opset or output type of a graph that computes the same, which
// onnxruntime runs instead.
const cases = [];
const add = (name, spec) => cases.push({ name, out: FLOAT, inits: {}, opset: 13, ...spec });

// The case `c`, whose one node writes `Y`, over halves: each of `floats` cast
// to f16 before the node, and its f16 result cast back. A node of halves
// computes in f32 and rounds its result once. onnxruntime drops that rounding
// where a Cast back to f32 follows, so it runs the graph that ends at the f16
// output.
const overHalves = (c, floats) => {
  const casts = floats.map((n) => node('Cast', [n], `H${n}`, { to: FLOAT16 }));
  const [only] = c.nodes;
  const writing = (output) => ({
    ...only,
    inputs: only.inputs.map((n) => (floats.includes(n) ? `H${n}` : n)),
    outputs: only.outputs.map((o) => (o === 'Y' ? output : o)),
  });
  return {
    ...c,
    nodes: [...casts, writing('S'), node('Cast', ['S'], 'Y', { to: FLOAT })],
    oracle: { nodes: [...casts, writing('Y')], out: FLOAT16 },
  };
};

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

add('Max', binary('Max', [2, 3], [3]));
add('Min', binary('Min', [2, 3], [2, 1]));
add('MaxOfThree', {
  inputs: { A: float([2, 3]), B: float([3]), C: float([2, 1]) },
  nodes: [node('Max', ['A', 'B', 'C'], 'Y')],
});
add('MinOfOne', { inputs: { A: float([2, 3]) }, nodes: [node('Min', ['A'], 'Y')] });

// Clip's bounds are scalars, each optional; a lower one past the upper yields
// the upper.
const clip = (bounds, names) => ({ inputs: { A: float([2, 3]) }, inits: bounds, nodes: [node('Clip', ['A', ...names], 'Y')] });
add('Clip', clip({ L: float([], [-1]), H: float([], [1.5]) }, ['L', 'H']));
add('ClipBelow', clip({ L: float([], [-1]) }, ['L']));
add('ClipAbove', clip({ H: float([], [1.5]) }, ['', 'H']));
add('ClipUnbounded', clip({}, []));
add('ClipByAttributes', {
  inputs: { A: float([2, 3]) },
  nodes: [node('Clip', ['A'], 'Y', { min: f32(-1), max: f32(1.5) })],
  opset: 6,
});
add('ClipCrossed', clip({ L: float([], [2]), H: float([], [-1]) }, ['L', 'H']));

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
// Neither a product past a half's range nor an alpha no half holds is rounded
// on the way.
const halfGemm = (a, b, c, attrs) => {
  const operands = ['A', 'B', ...(c ? ['C'] : [])];
  return overHalves({
    inputs: { A: float([1, 1], [a]), B: float([1, 1], [b]), ...(c ? { C: float([1, 1], [c]) } : {}) },
    nodes: [node('Gemm', operands, 'Y', attrs)],
  }, operands);
};
add('GemmHalfScaled', halfGemm(100, 100, null, { alpha: f32(1e-8) }));
add('GemmHalfScaledBack', halfGemm(256, 256, null, { alpha: f32(0.5) }));
add('GemmHalfBiasedBack', halfGemm(256, 256, -32768, {}));
add('GemmHalfBiasScaled', halfGemm(1, 1, 60000, { beta: f32(2), alpha: f32(-65000) }));
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
// The bias is added to a result past a half's range.
add('ConvHalfBiased', overHalves({
  inputs: { A: float([1, 1, 1, 2], [256, 1]), W: float([1, 1, 1, 1], [256]), B: float([1], [-32768]) },
  nodes: [node('Conv', ['A', 'W', 'B'], 'Y')],
}, ['A', 'W', 'B']));
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

// `halves` puts each lane's greatest at one index, and these move it about.
const scattered = (dims) => float(dims, Array.from({ length: dims.reduce((a, b) => a * b, 1) }, (_, i) => ((i * 5) % 7) - 3));
add('ArgMaxKept', { inputs: { A: scattered([2, 3, 4]) }, nodes: [node('ArgMax', ['A'], 'Y', { axis: 1 })], out: INT64 });
add('ArgMaxDropped', {
  inputs: { A: scattered([2, 3, 4]) },
  nodes: [node('ArgMax', ['A'], 'Y', { axis: -1, keepdims: 0 })],
  out: INT64,
});
add('ArgMaxFirst', { inputs: { A: scattered([3, 2]) }, nodes: [node('ArgMax', ['A'], 'Y')], out: INT64 });
// Each row's greatest twice, and an infinity: the first of them is the index.
add('ArgMaxTies', {
  inputs: { A: float([3, 4], [1, 3, 3, 0, -1, -2, -1, -3, -Infinity, Infinity, 2, Infinity]) },
  nodes: [node('ArgMax', ['A'], 'Y', { axis: 1 })],
  out: INT64,
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
// Past a half's range, a tie between two halves, and a value below its least.
add('CastThroughFloat16', cast(FLOAT16, FLOAT, [0.1, -65520, 65519, 1.00048828125, 1e-8, -0]));
add('CastThroughBFloat16', cast(BFLOAT16, FLOAT, [0.1, -65520, 3.4e38, 1.00390625, 1e-40, -0]));
// onnxruntime computes a float16 node in float32 and rounds where a `Cast`
// says to, so `a` holds values whose every intermediate is a half either way.
const inHalf = (a, nodes) => ({
  inputs: { A: float([2, 3], a) },
  inits: { C: float([3], [0.1, -65520, 1.00048828125]) },
  nodes: [node('Cast', ['A'], 'H', { to: FLOAT16 }), ...nodes, node('Cast', ['S'], 'Y', { to: FLOAT })],
});
// A constant cast at build time rounds as one cast at run does: 0.1 is no half.
const onto = [0, 0, 0, 0, 1, 2];
add('CastConstantToFloat16', inHalf(onto, [node('Cast', ['C'], 'D', { to: FLOAT16 }), node('Add', ['H', 'D'], 'S')]));
// onnxruntime leaves this one unrounded, so the constant holds halves.
add('CastLikeConstant', {
  ...inHalf(onto, [
    node('Constant', [], 'K', { value: { tensor: tensorProto('k', FLOAT, [3], new Float32Array([0.5, -2, 1.5]).buffer) } }),
    node('CastLike', ['K', 'H'], 'D'),
    node('Add', ['H', 'D'], 'S'),
  ]),
  opset: 15,
});
// A value the run computes, cast to a half and back as RMSNormalization's body does.
add('CastLikeComputed', {
  ...inHalf(onto, [
    node('Cast', ['H'], 'W', { to: FLOAT }),
    node('Mul', ['W', 'W'], 'Q'),
    node('CastLike', ['Q', 'H'], 'S'),
  ]),
  opset: 15,
});
// Where `erf` is 0, 1 or -1 to the last bit of either width.
add('GeluFloat16', { ...inHalf([0, 8, -8, 16, -16, 32], [node('Gelu', ['H'], 'S')]), opset: 20 });
// Where `erf` or `tanh` nears -1, one rounded to a half makes the result 0,
// and a cube past a half's range makes the tanh form's an infinity.
const halfGelu = (approximate) => overHalves({
  inputs: { A: float([2, 3], [-4, -3.5, -5, 3, 48, -48]) },
  nodes: [node('Gelu', ['A'], 'Y', { approximate })],
  opset: 20,
}, ['A']);
add('GeluHalfErf', halfGelu('none'));
add('GeluHalfTanh', halfGelu('tanh'));
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
// Squares past a half's range, normalized with each of `floats` a half.
const halfNorm = (floats) => overHalves({
  inputs: { A: float([2, 3], [300, -400, 500, 1, 2, 3]) },
  inits: { C: float([3], [1, -0.5, 2]) },
  nodes: [node('SimplifiedLayerNormalization', ['A', 'C'], 'Y', { epsilon: f32(1e-5), axis: -1 })],
  opset: 17,
}, floats);
// The default `stash_type` computes in f32.
add('SimplifiedLayerNormalizationHalf', halfNorm(['A', 'C']));
// The output takes the scale's element type, which may differ from the input's.
add('SimplifiedLayerNormalizationMixed', halfNorm(['C']));

// SkipSimplifiedLayerNormalization of A and its skip B, scaled by C and with a
// bias D where `bias`. `output` names which result the case checks: the
// normalization, 0, or the sum it normalizes, 3.
const skipNorm = ({ a, b, bias = false, output = 0 }) => {
  const inits = { C: float([3], [1, -0.5, 2]) };
  const operands = ['A', 'B', 'C'];
  if (bias) {
    inits.D = float([3], [0.5, -1, 0.25]);
    operands.push('D');
  }
  const outputs = ['O', '', '', 'Sum'];
  outputs[output] = 'Y';
  return {
    inputs: { A: float([2, 3], a), B: float([2, 3], b) },
    inits,
    nodes: [node('com.microsoft.SkipSimplifiedLayerNormalization', operands, outputs, { epsilon: f32(1e-5) })],
    opset: 17,
    domains: { 'com.microsoft': 1 },
  };
};
add('SkipNorm', skipNorm({}));
add('SkipNormBias', skipNorm({ bias: true }));
add('SkipNormSum', skipNorm({ bias: true, output: 3 }));
// Sums past a half's range: whether the normalization reads the sum rounded to
// a half or the one computed in f32.
const pastHalf = { a: [60000, 1, 2, 3, -60000, 1], b: [60000, 0, 0, 0, -60000, 0] };
add('SkipNormHalf', overHalves(skipNorm(pastHalf), ['A', 'B', 'C']));
add('SkipNormHalfSum', overHalves(skipNorm({ ...pastHalf, output: 3 }), ['A', 'B', 'C']));
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
// names which of the three results the case checks; `sink` a logit per query
// head, which joins each softmax's denominator. onnxruntime rotates a head of a
// multiple of 16 elements only.
const gqa = ({ s, past, heads, kv, dim, attrs = {}, rotary = false, bias = null, sink = null, output = 0 }) => {
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
  // Each optional operand at its index, an absent one before it named ''.
  const operands = ['Q', 'K', 'V', 'PK', 'PV', 'L', 'T'];
  const at = (index, name) => {
    while (operands.length < index) operands.push('');
    operands.push(name);
  };
  if (rotary) {
    inits.C = cache(Math.cos);
    inits.N = cache(Math.sin);
    at(7, 'C');
    at(8, 'N');
  }
  if (bias) {
    inputs.B = float([1, 1, s, total], bias);
    at(10, 'B');
  }
  if (sink) {
    inits.H = float([heads], sink);
    at(11, 'H');
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
const rotaryKey = gqa({ s: 2, past: 1, heads: 2, kv: 1, dim: 16, rotary: true, attrs: { do_rotary: 1 }, output: 1 });
add('GqaHalfRotaryPresentKey', overHalves(rotaryKey, ['Q', 'K', 'V', 'PK', 'PV', 'C', 'N']));
add('GqaPresentValue', gqa({ s: 2, past: 2, heads: 2, kv: 1, dim: 16, output: 2 }));
add('GqaWindow', gqa({ s: 5, past: 0, heads: 2, kv: 1, dim: 16, attrs: { local_window_size: 2 } }));
add('GqaWindowPast', gqa({ s: 2, past: 4, heads: 2, kv: 1, dim: 16, attrs: { local_window_size: 3 } }));
add('GqaScaleSoftcap', gqa({ s: 3, past: 1, heads: 2, kv: 1, dim: 16, attrs: { scale: f32(0.75), softcap: f32(1.5) } }));
add('GqaBias', gqa({ s: 2, past: 1, heads: 2, kv: 1, dim: 16, bias: [0, -3.4028234663852886e38, 0.5, 0, 0, -1] }));
add('GqaRotaryBias', gqa({
  s: 2, past: 1, heads: 2, kv: 1, dim: 16, rotary: true, attrs: { do_rotary: 1 }, bias: [0, 0, -2, -3.4028234663852886e38, 0, 0],
}));
// A sink above every score takes most of each softmax and one below takes
// little. Under a window, the sink joins the positions the window keeps.
add('GqaSink', gqa({ s: 3, past: 1, heads: 2, kv: 1, dim: 16, sink: [4, -2] }));
add('GqaSinkWindow', gqa({ s: 4, past: 2, heads: 2, kv: 1, dim: 16, attrs: { local_window_size: 2 }, sink: [0.5, 30] }));
add('GqaHalfSink', overHalves(gqa({ s: 2, past: 2, heads: 2, kv: 1, dim: 16, sink: [1, -1] }), ['Q', 'K', 'V', 'PK', 'PV', 'H']));

// Bytes spread over their range, apart for each `seed`, as packed weights.
const bytes = (dims, seed) => tensor(
  UINT8,
  dims,
  Array.from({ length: dims.reduce((a, b) => a * b, 1) }, (_, i) => (i * 37 + seed * 101) % 256),
);
const scales = (dims) => float(dims, Array.from({ length: dims.reduce((a, b) => a * b, 1) }, (_, i) => ((i % 5) - 2) / 4 || 0.125));

// MatMulNBits of `k` columns into 3 rows, of `bits` in blocks of `block`, with
// packed zero points where `zeros` and a bias where `bias`.
const nbits = ({ k, bits = 4, block = 32, zeros = true, bias = false }) => {
  const blocks = Math.ceil(k / block);
  const inits = { B: bytes([3, blocks, (block * bits) / 8], 1), S: scales([3, blocks]) };
  const operands = ['A', 'B', 'S'];
  if (zeros) {
    inits.Z = bytes([3, Math.ceil((blocks * bits) / 8)], 2);
    operands.push('Z');
  }
  if (bias) {
    inits.C = float([3], [0.5, -1, 2]);
    operands.push(...(zeros ? [] : ['']), '', 'C');
  }
  return {
    inputs: { A: float([1, 2, k]) },
    inits,
    nodes: [node('com.microsoft.MatMulNBits', operands, 'Y', { K: k, N: 3, bits, block_size: block })],
    domains: { 'com.microsoft': 1 },
  };
};
add('NBits', nbits({ k: 40 }));
add('NBitsBias', nbits({ k: 64, zeros: false, bias: true }));
add('NBits2', nbits({ k: 32, bits: 2, block: 16 }));
add('NBits8', nbits({ k: 16, bits: 8, block: 16, zeros: false }));
// Over halves, a product past a half's range that its bias brings back.
add('NBitsHalfBias', overHalves({
  inputs: { A: float([1, 1, 16], Array(16).fill(4096)) },
  inits: {
    B: tensor(UINT8, [3, 1, 16], Array(48).fill(129)),
    W: float([3, 1], [1, 1, 1]),
    C: float([3], [-32768, -32768, -32768]),
  },
  nodes: [node('com.microsoft.MatMulNBits', ['A', 'B', 'W', '', '', 'C'], 'Y', { K: 16, N: 3, bits: 8, block_size: 16 })],
  domains: { 'com.microsoft': 1 },
}, ['A', 'W', 'C']));

// QMoE of `tokens` rows of 32 values over 4 experts, `k` to a token, each a
// SwiGLU of 32 between two products in 4 bits and blocks of 32, its gate and
// up rows interleaved. `zeros` packs zero points and `bias` adds biases. The
// router's logits are apart in each row, so the experts a token picks are not
// left to a tie. A `layer` past the first names its router, its weights and
// what it reads and writes after itself, and draws them anew.
const qmoe = ({ tokens, k = 2, zeros = false, bias = true, attrs = {}, layer = 0 }) => {
  const [experts, hidden, inter] = [4, 32, 32];
  const named = (n) => (layer ? `${n}_${layer}` : n);
  const logits = Array.from({ length: tokens * experts }, (_, i) => (((i + layer) * 5) % 11) / 2 - 2.5 + i / 64);
  // Zero points of 0 to 3 under values of 0 to 15 leave nearly every value
  // positive, as the input and the scales are, so no sum cancels to a value
  // that f32 rounds far from the exact one, and the zero points alone set it.
  const rowScales = (dims) => zeros
    ? float(dims, Array.from({ length: dims.reduce((a, b) => a * b, 1) }, (_, i) => ((i % 5) + 1) / 512))
    : scales(dims);
  const small = (dims, seed) => {
    const t = bytes(dims, seed);
    return { ...t, values: t.values.map((b) => b & 0xf3) };
  };
  const seed = 10 * layer;
  const inits = {
    [named('W1')]: bytes([experts, 2 * inter, hidden / 2], 5 + seed),
    [named('S1')]: rowScales([experts, 2 * inter, 1]),
    [named('W2')]: bytes([experts, hidden, inter / 2], 6 + seed),
    [named('S2')]: rowScales([experts, hidden, 1]),
  };
  const operands = [named('X'), named('R'), named('W1'), named('S1'), '', named('W2'), named('S2'), ''];
  if (bias) {
    inits[named('B1')] = float([experts, 2 * inter], halves(experts * 2 * inter).map((v) => v / (layer + 1)));
    inits[named('B2')] = float([experts, hidden], halves(experts * hidden).map((v) => v / (layer + 2)));
    operands[4] = named('B1');
    operands[7] = named('B2');
  }
  if (zeros) {
    inits[named('Z1')] = small([experts, 2 * inter, 1], 7 + seed);
    inits[named('Z2')] = small([experts, hidden, 1], 8 + seed);
    operands.push('', '', '', named('Z1'), named('Z2'));
  }
  const x = halves(tokens * hidden).map((v) => (zeros ? Math.abs(v) + 0.5 : v));
  return {
    inputs: { [named('X')]: float([tokens, hidden], x), [named('R')]: float([tokens, experts], logits) },
    inits,
    nodes: [node('com.microsoft.QMoE', operands, named('Y'), {
      k,
      activation_type: 'swiglu',
      swiglu_fusion: 1,
      activation_alpha: f32(1.702),
      activation_beta: f32(1),
      swiglu_limit: f32(7),
      normalize_routing_weights: 1,
      expert_weight_bits: 4,
      block_size: 32,
      ...attrs,
    })],
    domains: { 'com.microsoft': 1 },
  };
};
// The QMoE nodes of case `c` computed from their definition in f64, in order,
// each over the tensors `given` names and those the nodes before it wrote.
const qmoeExact = (c, given) => {
  const known = { ...given };
  let y;
  for (const n of c.nodes) {
    y = qmoeOf(n, known);
    known[n.outputs[0]] = { dims: y.dims, values: y.data };
  }
  return y;
};
// The QMoE node `n` from its definition: each token's `k` largest logits pick
// its experts, and each pick weighs its expert's output by its softmax over
// every expert, divided by the picks' sum where `normalize_routing_weights` is
// set. Each expert is a SwiGLU of interleaved gate and up rows between two
// products of weights packed in blocks.
const qmoeOf = (n, given) => {
  const { inputs: ops, attrs } = n;
  const operand = (i) => (ops[i] ? given[ops[i]] : null);
  const number = (a, fallback) => (a === undefined ? fallback : (a.f32 ?? a));
  const [x, router] = [operand(0), operand(1)];
  const [bits, block, k] = [attrs.expert_weight_bits, attrs.block_size, attrs.k];
  const [alpha, beta] = [number(attrs.activation_alpha), number(attrs.activation_beta)];
  const limit = number(attrs.swiglu_limit, Infinity);
  const [hidden, experts] = [x.dims.at(-1), router.dims.at(-1)];
  const tokens = x.values.length / hidden;
  const [per, mask] = [8 / bits, (1 << bits) - 1];
  const packedAt = (values, at, i) => (values[at + Math.floor(i / per)] >> ((i % per) * bits)) & mask;
  // `rows` rows of expert `e`'s weight at `w`, each `width` values, by `input`.
  const product = (w, rows, width, e, input) => {
    const [weight, scale, bias, zeros] = [operand(w), operand(w + 1), operand(w + 2), operand(w === 2 ? 11 : 12)];
    const blocks = Math.ceil(width / block);
    const zeroBytes = Math.ceil(blocks / per);
    return Array.from({ length: rows }, (_, n) => {
      const row = e * rows + n;
      let sum = 0;
      for (let i = 0; i < width; i++) {
        const b = Math.floor(i / block);
        const zero = zeros ? packedAt(zeros.values, row * zeroBytes, b) : 1 << (bits - 1);
        const q = packedAt(weight.values, row * (width / per), i);
        sum += input[i] * (q - zero) * scale.values[row * blocks + b];
      }
      return sum + (bias ? bias.values[row] : 0);
    });
  };
  const inner = operand(2).dims[1] / 2;
  const data = [];
  for (let t = 0; t < tokens; t++) {
    const logits = router.values.slice(t * experts, (t + 1) * experts);
    const picks = [...logits.keys()].sort((a, b) => logits[b] - logits[a] || a - b).slice(0, k);
    const top = Math.max(...logits);
    const p = logits.map((l) => Math.exp(l - top));
    const total = attrs.normalize_routing_weights ? picks.reduce((s, e) => s + p[e], 0) : p.reduce((s, v) => s + v);
    const row = x.values.slice(t * hidden, (t + 1) * hidden);
    const y = Array(hidden).fill(0);
    for (const e of picks) {
      const h = product(2, 2 * inner, hidden, e, row);
      const a = Array.from({ length: inner }, (_, i) => {
        const g = Math.min(h[2 * i], limit);
        const u = Math.max(Math.min(h[2 * i + 1], limit), -limit);
        return (g / (1 + Math.exp(-alpha * g))) * (u + beta);
      });
      product(5, hidden, inner, e, a).forEach((v, n) => {
        y[n] += (p[e] / total) * v;
      });
    }
    data.push(...y);
  }
  return { dims: x.dims, data };
};
// More picks than experts: each expert's rows, masked.
add('QMoePrompt', qmoe({ tokens: 3 }));
// One token picks fewer experts than there are: those experts alone.
add('QMoeStep', qmoe({ tokens: 1 }));
// onnxruntime weighs the picks as normalized whether or not
// `normalize_routing_weights` is set, so the definition computes this one.
add('QMoeUnnormalized', { ...qmoe({ tokens: 3, attrs: { normalize_routing_weights: 0 } }), exact: qmoeExact });
add('QMoeZeroPoints', qmoe({ tokens: 3, zeros: true, bias: false }));
add('QMoeHalf', overHalves(qmoe({ tokens: 3 }), ['X', 'R', 'S1', 'B1', 'S2', 'B2']));
add('QMoeHalfStep', overHalves(qmoe({ tokens: 1 }), ['X', 'R', 'S1', 'B1', 'S2', 'B2']));
// Two QMoE layers of one layout, the second over the first's output: a model
// whose layers share pools, which hold as few experts as a budget sets.
const qmoeLayers = (tokens) => {
  const [first, second] = [qmoe({ tokens }), qmoe({ tokens, layer: 1 })];
  const [[a], [b]] = [first.nodes, second.nodes];
  return {
    inputs: { X: first.inputs.X, R: first.inputs.R, R_1: second.inputs.R_1 },
    inits: { ...first.inits, ...second.inits },
    nodes: [
      { ...a, outputs: ['H'] },
      { ...b, inputs: b.inputs.map((n) => (n === 'X_1' ? 'H' : n)), outputs: ['Y'] },
    ],
    domains: first.domains,
  };
};
// onnxruntime parts from the definition over two layers far more than over one,
// so the definition computes this one.
add('QMoeLayers', { ...qmoeLayers(3), exact: qmoeExact });

// GatherBlockQuantized of a table of 5 rows of `width` values in blocks of
// 32, by indices counting from the end and past it.
const gatherBlocks = ({ width, zeros = true, attrs = { gather_axis: 0, quantize_axis: 1 } }) => {
  const blocks = Math.ceil(width / 32);
  const inits = { D: bytes([5, width / 2], 3), S: scales([5, blocks]) };
  const operands = ['D', 'I', 'S'];
  if (zeros) {
    inits.Z = bytes([5, Math.ceil(blocks / 2)], 4);
    operands.push('Z');
  }
  return {
    inputs: { I: int64([2, 2], [4, -1, 0, 7]) },
    inits,
    nodes: [node('com.microsoft.GatherBlockQuantized', operands, 'Y', { bits: 4, block_size: 32, ...attrs })],
    domains: { 'com.microsoft': 1 },
  };
};
add('GatherBlocks', gatherBlocks({ width: 40 }));
add('GatherBlocksCentered', gatherBlocks({ width: 64, zeros: false }));
add('GatherBlocksDefaultAxes', gatherBlocks({ width: 32, attrs: {} }));

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
  if (type === UINT8) return new Uint8Array(values).buffer;
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
  const given = { ...c.inits };
  for (const [i, t] of Object.entries(c.inputs)) {
    const count = t.dims.reduce((a, b) => a * b, 1);
    const values = t.values ?? halves(count);
    feeds[i] = feed(t.type, t.dims, values);
    given[i] = { ...t, values };
    lines.push(line(['input', i, elementName[t.type], list(t.dims), list(spelled(t.type, values))]));
  }
  let y;
  if (c.exact) {
    y = c.exact(c, given);
  } else {
    const session = await ort.InferenceSession.create(c.oracle ? model({ ...c, ...c.oracle }) : path);
    y = (await session.run(feeds, ['Y'])).Y;
  }
  lines.push(line(['output', elementName[c.out], list(y.dims), list(spelled(c.out, y.data))]));
  writeFileSync(join(here, 'operators', `${c.name}.txt`), lines.join(''));
}
