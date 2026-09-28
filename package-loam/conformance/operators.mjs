// Writes operators/<case>.onnx beside this script, one model per case of an
// operator Loam lowers, over static shapes and with the attributes and edge
// values where an implementation's reading is least safe. Beside each goes
// <case>.txt: the axes the case names each tensor's by, the inputs it fed, and
// what onnxruntime computed from them, which operators_test.wado holds Loam to.
//
//   npm install onnxruntime-node@1.30.0
//   node operators.mjs <this directory>
import { mkdirSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import * as ort from 'onnxruntime-node';
import {
  BFLOAT16, BOOL, FLOAT, FLOAT16, INT32, INT64, f32, modelProto, node, nodeProto, tensorProto, text, valueInfo,
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

// Each case computes `Y`. `inputs` are fed, `inits` are initializers, and
// `layout` names the axes of each tensor that needs them, by tensor name. A case
// whose opset onnxruntime does not run names in `oracle` the nodes, initializers
// and opset of a graph that computes the same, which onnxruntime runs instead.
const cases = [];
const add = (name, spec) => cases.push({ name, out: FLOAT, inits: {}, opset: 13, ...spec });

const pair = (a, b) => ({ A: float(a), B: float(b) });
const binary = (op, a, b, layout) => ({ inputs: pair(a, b), nodes: [node(op, ['A', 'B'], 'Y')], layout });
add('AddSame', binary('Add', [2, 3], [2, 3], { A: ['Row', 'Col'], B: ['Row', 'Col'] }));
add('AddSuffix', binary('Add', [2, 3], [3], { A: ['Row', 'Col'], B: ['Col'] }));
add('AddPrefix', binary('Add', [3], [2, 3], { A: ['Col'], B: ['Row', 'Col'] }));
add('MulStretchRight', binary('Mul', [2, 3], [2, 1], { A: ['Row', 'Col'], B: ['Row', 'Col'] }));
add('SubStretchLeft', binary('Sub', [1, 3], [2, 3], { A: ['Row', 'Col'], B: ['Row', 'Col'] }));
add('AddStretchBoth', binary('Add', [2, 1], [1, 3], { A: ['Row', 'Col'], B: ['Row', 'Col'], Y: ['Row', 'Col'] }));
// An axis of 1 the layout names apart from the axis it stretches over.
add('MulStretchRenamed', binary('Mul', [2, 1], [2, 3], { A: ['Row', 'One'], B: ['Row', 'Col'], Y: ['Row', 'Col'] }));
add('SubStretchRenamedRanks', binary('Sub', [3, 1, 4], [2, 4], { A: ['Depth', 'One', 'Col'], B: ['Row', 'Col'], Y: ['Depth', 'Row', 'Col'] }));
add('DivByZero', {
  inputs: { A: float([2, 3], [1, -1, 0, 3, -3, 0.5]), B: float([2, 3], [0, 0, 0, 2, -0.5, 4]) },
  nodes: [node('Div', ['A', 'B'], 'Y')],
  layout: { A: ['Row', 'Col'], B: ['Row', 'Col'] },
});
add('PowTensor', binary('Pow', [2, 3], [2, 3], { A: ['Row', 'Col'], B: ['Row', 'Col'] }));
add('PowHalf', {
  inputs: { A: float([2, 3]) },
  inits: { E: float([], [0.5]) },
  nodes: [node('Pow', ['A', 'E'], 'Y')],
  layout: { A: ['Row', 'Col'] },
});
add('SubFromScalar', {
  inputs: { A: float([2, 3]) },
  inits: { E: float([], [1.5]) },
  nodes: [node('Sub', ['E', 'A'], 'Y')],
  layout: { A: ['Row', 'Col'] },
});

for (const op of ['Relu', 'Sigmoid', 'Tanh', 'Erf', 'Exp', 'Log', 'Sqrt', 'Neg', 'Abs', 'Identity']) {
  add(op, { inputs: { A: float([2, 3]) }, nodes: [node(op, ['A'], 'Y')], layout: { A: ['Row', 'Col'] } });
}

const mm = { A: ['Row', 'K'], B: ['K', 'Col'] };
add('MatMul', binary('MatMul', [2, 3], [3, 4], mm));
add('MatMulBatched', binary('MatMul', [2, 2, 3], [2, 3, 4], { A: ['Batch', 'Row', 'K'], B: ['Batch', 'K', 'Col'] }));
add('MatMulShared', binary('MatMul', [2, 2, 3], [3, 4], { A: ['Batch', 'Row', 'K'], B: ['K', 'Col'] }));
add('MatMulRowVector', binary('MatMul', [3], [3, 4], { A: ['K'], B: ['K', 'Col'] }));
add('MatMulColumnVector', binary('MatMul', [2, 3], [3], { A: ['Row', 'K'], B: ['K'] }));
add('MatMulBroadcastBatch', binary('MatMul', [1, 2, 3], [2, 3, 4], { A: ['Batch', 'Row', 'K'], B: ['Batch', 'K', 'Col'] }));

add('Gemm', {
  inputs: { A: float([2, 3]), B: float([3, 4]), C: float([4]) },
  nodes: [node('Gemm', ['A', 'B', 'C'], 'Y')],
  layout: { ...mm, C: ['Col'] },
});
add('GemmTransposedScaled', {
  inputs: { A: float([3, 2]), B: float([4, 3]), C: float([1, 4]) },
  nodes: [node('Gemm', ['A', 'B', 'C'], 'Y', { alpha: f32(0.5), beta: f32(2), transA: 1, transB: 1 })],
  layout: { A: ['K', 'Row'], B: ['Col', 'K'], C: ['Row', 'Col'] },
});
add('GemmScalarBias', {
  inputs: { A: float([2, 3]), B: float([3, 4]), C: float([]) },
  nodes: [node('Gemm', ['A', 'B', 'C'], 'Y')],
  layout: mm,
});

const cube = { A: ['Row', 'Col', 'Depth'] };
add('TransposeReversed', { inputs: { A: float([2, 3, 4]) }, nodes: [node('Transpose', ['A'], 'Y')], layout: cube });
add('TransposeSwap', { inputs: { A: float([2, 3, 4]) }, nodes: [node('Transpose', ['A'], 'Y', { perm: [1, 0, 2] })], layout: cube });
add('TransposeCycle', { inputs: { A: float([2, 3, 4]) }, nodes: [node('Transpose', ['A'], 'Y', { perm: [2, 0, 1] })], layout: cube });

add('ReshapeKeepRest', {
  inputs: { A: float([2, 3, 4]) },
  inits: { S: int64([2], [0, -1]) },
  nodes: [node('Reshape', ['A', 'S'], 'Y')],
  layout: cube,
});
add('ReshapeSplit', {
  inputs: { A: float([2, 12]) },
  inits: { S: int64([3], [2, 3, 4]) },
  nodes: [node('Reshape', ['A', 'S'], 'Y')],
  layout: { A: ['Row', 'Wide'], Y: ['Row', 'Col', 'Depth'] },
});
add('FlattenAll', { inputs: { A: float([2, 3, 4]) }, nodes: [node('Flatten', ['A'], 'Y', { axis: 0 })], layout: { ...cube, Y: ['One', 'All'] } });
add('FlattenLast', { inputs: { A: float([2, 3, 4]) }, nodes: [node('Flatten', ['A'], 'Y', { axis: -1 })], layout: cube });
add('UnsqueezeEnds', {
  inputs: { A: float([2, 3]) },
  inits: { X: int64([2], [-1, 0]) },
  nodes: [node('Unsqueeze', ['A', 'X'], 'Y')],
  layout: { A: ['Row', 'Col'], Y: ['Lead', 'Row', 'Col', 'Trail'] },
});
add('SqueezeAll', {
  inputs: { A: float([1, 3, 1]) },
  nodes: [node('Squeeze', ['A'], 'Y')],
  layout: { A: ['One', 'Col', 'Unit'] },
});
add('SqueezeLast', {
  inputs: { A: float([1, 3, 1]) },
  inits: { X: int64([1], [-1]) },
  nodes: [node('Squeeze', ['A', 'X'], 'Y')],
  layout: { A: ['One', 'Col', 'Unit'] },
});

add('SplitEqual', {
  inputs: { A: float([2, 6]) },
  nodes: [node('Split', ['A'], ['Y', 'P', 'Q'], { axis: 1 })],
  layout: { A: ['Row', 'Wide'], Y: ['Row', 'Part'], P: ['Row', 'Part'], Q: ['Row', 'Part'] },
});
add('SplitSizes', {
  inputs: { A: float([2, 7]) },
  inits: { S: int64([2], [2, 5]) },
  nodes: [node('Split', ['A', 'S'], ['P', 'Y'], { axis: -1 })],
  layout: { A: ['Row', 'Wide'], P: ['Row', 'Head'], Y: ['Row', 'Tail'] },
});

const image = (h, w) => ({ A: float([1, 2, h, w]) });
const pooled = { A: ['Batch', 'Chan', 'H', 'W'], Y: ['Batch', 'Chan', 'OutH', 'OutW'] };
const pool = (h, w, attrs) => ({ inputs: image(h, w), nodes: [node('MaxPool', ['A'], 'Y', attrs)], layout: pooled });
add('MaxPool', pool(4, 4, { kernel_shape: [2, 2], strides: [2, 2] }));
add('MaxPoolPadded', pool(4, 4, { kernel_shape: [3, 3], pads: [1, 1, 1, 1] }));
add('MaxPoolCeil', pool(5, 5, { kernel_shape: [2, 2], strides: [2, 2], ceil_mode: 1 }));
add('MaxPoolSameUpper', pool(5, 5, { kernel_shape: [3, 3], strides: [2, 2], auto_pad: 'SAME_UPPER' }));
add('MaxPoolSameLower', pool(4, 4, { kernel_shape: [2, 2], auto_pad: 'SAME_LOWER' }));
add('MaxPoolDilated', pool(5, 5, { kernel_shape: [2, 2], dilations: [2, 2] }));
add('MaxPool1D', {
  inputs: { A: float([1, 2, 7]) },
  nodes: [node('MaxPool', ['A'], 'Y', { kernel_shape: [3], strides: [2] })],
  layout: { A: ['Batch', 'Chan', 'W'], Y: ['Batch', 'Chan', 'OutW'] },
});

const convolved = { A: ['Batch', 'Chan', 'H', 'W'], W: ['Out', 'Chan', 'KH', 'KW'], B: ['Out'], Y: ['Batch', 'Out', 'OutH', 'OutW'] };
add('Conv', {
  inputs: { A: float([1, 2, 4, 4]), W: float([3, 2, 3, 3]), B: float([3]) },
  nodes: [node('Conv', ['A', 'W', 'B'], 'Y', { pads: [1, 1, 1, 1] })],
  layout: convolved,
});
add('ConvStridedDilated', {
  inputs: { A: float([1, 2, 7, 7]), W: float([3, 2, 2, 2]) },
  nodes: [node('Conv', ['A', 'W'], 'Y', { strides: [2, 2], dilations: [2, 2] })],
  layout: convolved,
});
add('ConvSameLower', {
  inputs: { A: float([1, 2, 4, 4]), W: float([3, 2, 2, 2]) },
  nodes: [node('Conv', ['A', 'W'], 'Y', { auto_pad: 'SAME_LOWER' })],
  layout: convolved,
});
add('Conv1D', {
  inputs: { A: float([1, 1, 6]), W: float([2, 1, 3]) },
  nodes: [node('Conv', ['A', 'W'], 'Y')],
  layout: { A: ['Batch', 'Chan', 'W'], W: ['Out', 'Chan', 'K'], Y: ['Batch', 'Out', 'OutW'] },
});
add('ConvGrouped', {
  inputs: { A: float([1, 4, 3, 3]), W: float([2, 2, 2, 2]) },
  nodes: [node('Conv', ['A', 'W'], 'Y', { group: 2 })],
  layout: { A: ['Batch', 'Chan', 'H', 'W'], W: ['Out', 'Group', 'KH', 'KW'], Y: ['Batch', 'Out', 'OutH', 'OutW'] },
});
add('ConvDepthwise', {
  inputs: { A: float([1, 3, 4, 4]), W: float([3, 1, 3, 3]), B: float([3]) },
  nodes: [node('Conv', ['A', 'W', 'B'], 'Y', { group: 3, pads: [1, 1, 1, 1] })],
  layout: { ...convolved, W: ['Out', 'One', 'KH', 'KW'] },
});

add('SoftmaxLast', { inputs: { A: float([2, 3]) }, nodes: [node('Softmax', ['A'], 'Y')], layout: { A: ['Row', 'Col'] } });
add('SoftmaxFirst', { inputs: { A: float([2, 3]) }, nodes: [node('Softmax', ['A'], 'Y', { axis: 0 })], layout: { A: ['Row', 'Col'] } });
// Before opset 7, arithmetic with `broadcast` aligned B at A's `axis`, which a
// B unsqueezed along the axes after it restates.
add('AddAtAxis', {
  inputs: pair([2, 3, 4], [3]),
  nodes: [node('Add', ['A', 'B'], 'Y', { broadcast: 1, axis: 1 })],
  layout: { ...cube, B: ['Col'] },
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
  layout: { ...cube, B: ['Row', 'Col'] },
  opset: 6,
  oracle: {
    inits: { S: int64([1], [2]) },
    nodes: [node('Unsqueeze', ['B', 'S'], 'U'), node('Sub', ['A', 'U'], 'Y')],
    opset: 13,
  },
});

// Before opset 13, Softmax normalized every axis from `axis` on as one lane.
add('SoftmaxFlattenedDefault', { inputs: { A: float([2, 3, 4]) }, nodes: [node('Softmax', ['A'], 'Y')], layout: cube, opset: 11 });
add('SoftmaxFlattenedFirst', {
  inputs: { A: float([2, 3, 4]) },
  nodes: [node('Softmax', ['A'], 'Y', { axis: 0 })],
  layout: cube,
  opset: 11,
});
add('SoftmaxMiddle', { inputs: { A: float([2, 3, 4]) }, nodes: [node('Softmax', ['A'], 'Y', { axis: 1 })], layout: cube });
add('SoftmaxLarge', {
  inputs: { A: float([2, 3], [1000, 1001, 1002, -1000, 0, 1000]) },
  nodes: [node('Softmax', ['A'], 'Y')],
  layout: { A: ['Row', 'Col'] },
});

add('ReduceMeanKept', { inputs: { A: float([2, 3, 4]) }, nodes: [node('ReduceMean', ['A'], 'Y', { axes: [1] })], layout: cube });
add('ReduceMeanDropped', {
  inputs: { A: float([2, 3, 4]) },
  nodes: [node('ReduceMean', ['A'], 'Y', { axes: [-1], keepdims: 0 })],
  layout: cube,
});
add('ReduceMeanAll', { inputs: { A: float([2, 3, 4]) }, nodes: [node('ReduceMean', ['A'], 'Y')], layout: cube });
add('ReduceSumLast', {
  inputs: { A: float([2, 3, 4]) },
  inits: { X: int64([1], [-1]) },
  nodes: [node('ReduceSum', ['A', 'X'], 'Y')],
  layout: cube,
});
add('ReduceSumNoop', {
  inputs: { A: float([2, 3]) },
  nodes: [node('ReduceSum', ['A'], 'Y', { noop_with_empty_axes: 1 })],
  layout: { A: ['Row', 'Col'] },
});

// Loam holds no bool at the boundary, so the condition is cast from a float.
add('WhereBroadcast', {
  inputs: { C: float([2, 3], [1, 0, -2, 0, 0, 0.5]), A: float([2, 3]), B: float([3]) },
  nodes: [node('Cast', ['C'], 'M', { to: BOOL }), node('Where', ['M', 'A', 'B'], 'Y')],
  layout: { C: ['Row', 'Col'], A: ['Row', 'Col'], B: ['Col'] },
});

add('GatherRows', {
  inputs: { A: float([4, 3]), I: int64([2], [3, -1]) },
  nodes: [node('Gather', ['A', 'I'], 'Y')],
  layout: { A: ['Vocab', 'Col'], I: ['Pick'] },
});
add('GatherScalarIndex', {
  inputs: { A: float([4, 3]) },
  inits: { I: int64([], [1]) },
  nodes: [node('Gather', ['A', 'I'], 'Y')],
  layout: { A: ['Vocab', 'Col'] },
});
add('GatherMatrixIndices', {
  inputs: { A: float([4, 3]), I: int64([2, 2], [0, 1, 3, 2]) },
  nodes: [node('Gather', ['A', 'I'], 'Y')],
  layout: { A: ['Vocab', 'Col'], I: ['Batch', 'Pick'] },
});
add('GatherColumns', {
  inputs: { A: float([4, 3]), I: int64([2], [2, 0]) },
  nodes: [node('Gather', ['A', 'I'], 'Y', { axis: 1 })],
  layout: { A: ['Row', 'Vocab'], I: ['Pick'] },
});

add('SliceBackByTwo', {
  inputs: { A: float([4, 5]) },
  inits: { B: int64([1], [-1]), E: int64([1], [-10000]), X: int64([1], [-1]), P: int64([1], [-2]) },
  nodes: [node('Slice', ['A', 'B', 'E', 'X', 'P'], 'Y')],
  layout: { A: ['Row', 'Col'], Y: ['Row', 'Part'] },
});
add('SliceTwoAxes', {
  inputs: { A: float([4, 5]) },
  inits: { B: int64([2], [1, 0]), E: int64([2], [3, 5]), X: int64([2], [0, 1]), P: int64([2], [1, 2]) },
  nodes: [node('Slice', ['A', 'B', 'E', 'X', 'P'], 'Y')],
  layout: { A: ['Row', 'Col'], Y: ['Rows', 'Cols'] },
});

// A bool or an int32 is cast back to what the boundary holds.
const cast = (to, back, values) => ({
  inputs: { A: float([2, 3], values) },
  nodes: back === undefined
    ? [node('Cast', ['A'], 'Y', { to })]
    : [node('Cast', ['A'], 'M', { to }), node('Cast', ['M'], 'Y', { to: back })],
  layout: { A: ['Row', 'Col'] },
  out: back ?? to,
});
add('CastToInt64', cast(INT64));
add('CastToBool', cast(BOOL, FLOAT, [0, -0, 0.5, -3.5, Infinity, NaN]));
add('CastBoolToInt64', cast(BOOL, INT64, [0, -0, 0.5, -3.5, Infinity, NaN]));
add('CastToInt32', cast(INT32, INT64));
add('CastFromInt64', {
  inputs: { A: int64([2, 3], [-3, -1, 0, 1, 2, 9007199254740993]) },
  nodes: [node('Cast', ['A'], 'Y', { to: FLOAT })],
  layout: { A: ['Row', 'Col'] },
});

add('ConstantOfShape', {
  inputs: { A: float([2, 3]) },
  nodes: [
    node('Shape', ['A'], 'S'),
    node('ConstantOfShape', ['S'], 'Y', { value: { tensor: tensorProto('v', FLOAT, [1], new Float32Array([2.5]).buffer) } }),
  ],
  layout: { A: ['Row', 'Col'], Y: ['Row', 'Col'] },
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
  layout: { A: ['Row', 'Col'], C: ['Col'], D: ['Col'] },
});
add('ConstantFloat16', halfConstant(FLOAT16, [0x3e00, 0xc000, 0x2e66, 0x0001]));
add('ConstantBFloat16', halfConstant(BFLOAT16, [0x3fc0, 0xc000, 0x3dcd, 0x0001]));
add('ShapeWindow', {
  inputs: { A: float([2, 3, 4]) },
  nodes: [node('Shape', ['A'], 'Y', { start: 1, end: -1 })],
  layout: { ...cube, Y: ['Dim'] },
  out: INT64,
  opset: 15,
});
add('ConcatShapes', {
  inputs: { A: float([2, 3]), B: float([4]) },
  nodes: [node('Shape', ['A'], 'S'), node('Shape', ['B'], 'T'), node('Concat', ['S', 'T'], 'Y', { axis: 0 })],
  layout: { A: ['Row', 'Col'], B: ['Wide'], Y: ['Dim'] },
  out: INT64,
});

const elementName = { [FLOAT]: 'f32', [INT64]: 'i64' };

// The tensor's elements as the .txt spells them: a float by its bits, so that
// the case reads back exactly what was fed and computed.
function spelled(type, data) {
  if (type === FLOAT) return Array.from(new Uint32Array(Float32Array.from(data).buffer));
  return Array.from(data, (v) => v.toString());
}

const line = (fields) => `${fields.join(' ')}\n`;
const list = (values) => `[${values.join(',')}]`;

const raw = (type, values) => (type === FLOAT ? new Float32Array(values) : new BigInt64Array(values)).buffer;

const feed = (type, dims, values) => type === FLOAT
  ? new ort.Tensor('float32', Float32Array.from(values), dims)
  : new ort.Tensor('int64', BigInt64Array.from(values), dims);

// GraphProto: node = 1, name = 2, initializer = 5, input = 11, output = 12.
function model({ name, inputs, inits, nodes, out, opset }) {
  const graph = [
    ...nodes.map((n) => [1, nodeProto(n)]),
    [2, text(name)],
    ...Object.entries(inits).map(([i, t]) => [5, tensorProto(i, t.type, t.dims, raw(t.type, t.values))]),
    ...Object.entries(inputs).map(([i, t]) => [11, valueInfo(i, t.type, t.dims)]),
    [12, valueInfo('Y', out, null)],
  ];
  return modelProto(graph, opset);
}

mkdirSync(join(here, 'operators'), { recursive: true });
for (const c of cases) {
  const path = join(here, 'operators', `${c.name}.onnx`);
  writeFileSync(path, model(c));
  const lines = Object.entries(c.layout).map(([t, axes]) => line(['layout', t, ...axes]));
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
