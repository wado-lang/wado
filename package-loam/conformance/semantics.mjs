// Writes semantics/<case>.onnx beside this script, one model per case of
// ONNX's shape arithmetic over a length only the run knows, `N` = the length of
// `X`. Beside each goes <case>.txt: what onnxruntime computes from it at each
// of `lengths`, a line each, which semantics_test.wado holds Loam to. `M`
// holds 8 elements, so the second length runs past it.
//
//   npm install onnxruntime-node@1.30.0
//   node semantics.mjs <this directory>
import { mkdirSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import * as ort from 'onnxruntime-node';
import { FLOAT, INT32, INT64, message, tensorProto, text } from '../tests/onnx_proto.mjs';

const here = process.argv[2] ?? dirname(fileURLToPath(import.meta.url));

const lengths = [3, 10];

// The int64 constants the cases compute from, each of one element.
const constants = {
  Zero: 0n,
  One: 1n,
  Two: 2n,
  Eight: 8n,
  MinusOne: -1n,
  MinusTwo: -2n,
  MinusNine: -9n,
  MinusFar: -4000000000n,
  Big: 3000000000n,
};

// Every case reads `S = Shape(X)`, and the nodes it lists after that.
const node = (op, inputs, output, attrs = []) => ({ op, inputs, output, attrs });
const cases = [
  ['SliceToLength', [node('Slice', ['M', 'Zero', 'S'], 'SliceToLength')]],
  ['SliceFromMinusLength', [
    node('Sub', ['Zero', 'S'], 'MinusS'),
    node('Slice', ['M', 'MinusS', 'Eight'], 'SliceFromMinusLength'),
  ]],
  ['SliceFromPastEnd', [
    node('Add', ['S', 'Eight'], 'SPlusEight'),
    node('Slice', ['M', 'SPlusEight', 'Eight'], 'SliceFromPastEnd'),
  ]],
  ['SliceBackFromLength', [node('Slice', ['M', 'S', 'Zero', 'Zero', 'MinusOne'], 'SliceBackFromLength')]],
  ['SliceBackToFront', [node('Slice', ['M', 'S', 'MinusNine', 'Zero', 'MinusOne'], 'SliceBackToFront')]],
  ['SliceByTwoToLength', [node('Slice', ['M', 'Zero', 'S', 'Zero', 'Two'], 'SliceByTwoToLength')]],
  ['SliceDropLast', [
    node('Sub', ['S', 'One'], 'SMinusOne'),
    node('Slice', ['X', 'Zero', 'SMinusOne'], 'SliceDropLast'),
  ]],
  ['SliceLast', [node('Slice', ['X', 'MinusOne', 'S'], 'SliceLast')]],
  ['SliceReversed', [node('Slice', ['X', 'MinusOne', 'MinusFar', 'Zero', 'MinusOne'], 'SliceReversed')]],
  ['RangeToLength', [node('Range', ['Zero', 'S', 'One'], 'RangeToLength')]],
  ['RangeByTwoToLength', [node('Range', ['Zero', 'S', 'Two'], 'RangeByTwoToLength')]],
  ['RangeBackFromLength', [node('Range', ['S', 'Zero', 'MinusOne'], 'RangeBackFromLength')]],
  ['RangeBackByTwo', [node('Range', ['S', 'Zero', 'MinusTwo'], 'RangeBackByTwo')]],
  ['CastWraps', [
    node('Cast', ['Big'], 'BigInt32', [['to', INT32]]),
    node('Cast', ['BigInt32'], 'CastWraps', [['to', INT64]]),
  ]],
  ['CastWrapsLength', [
    node('Mul', ['S', 'Big'], 'SBig'),
    node('Cast', ['SBig'], 'SBigInt32', [['to', INT32]]),
    node('Cast', ['SBigInt32'], 'CastWrapsLength', [['to', INT64]]),
  ]],
  ['ReshapeKeepSplit', [
    node('Concat', ['Zero', 'Two', 'MinusOne'], 'KeepSplitShape', [['axis', 0]]),
    node('Reshape', ['P', 'KeepSplitShape'], 'ReshapeKeepSplit'),
  ]],
  ['ReshapeFlat', [node('Reshape', ['P', 'MinusOne'], 'ReshapeFlat')]],
  ['ReshapeRestFirst', [
    node('Concat', ['MinusOne', 'Two'], 'RestFirstShape', [['axis', 0]]),
    node('Reshape', ['P', 'RestFirstShape'], 'ReshapeRestFirst'),
  ]],
];

// ValueInfoProto: name = 1, type = 2; TypeProto.tensor_type = 1;
// Tensor: elem_type = 1, shape = 2; TensorShapeProto.dim = 1;
// Dimension: dim_value = 1, dim_param = 2.
function valueInfo(name, elemType, dims) {
  const dim = (d) => [1, typeof d === 'string' ? [[2, text(d)]] : [[1, d]]];
  const shape = dims === null ? [] : [[2, dims.map(dim)]];
  return [[1, text(name)], [2, [[1, [[1, elemType], ...shape]]]]];
}

function int64Initializer(name, value) {
  return tensorProto(name, INT64, [1], new Uint8Array(new BigInt64Array([value]).buffer));
}

// NodeProto: input = 1, output = 2, op_type = 4, attribute = 5;
// AttributeProto: name = 1, i = 3, type = 20 (INT = 2).
function nodeProto({ op, inputs, output, attrs }) {
  return [
    ...inputs.map((i) => [1, text(i)]),
    [2, text(output)],
    [4, text(op)],
    ...attrs.map(([name, i]) => [5, [[1, text(name)], [3, i], [20, 2]]]),
  ];
}

const inputs = {
  X: { dims: ['N'], feed: (n) => new ort.Tensor('float32', iota(n).map((v) => v + 10), [n]) },
  M: { dims: [8], feed: () => new ort.Tensor('float32', iota(8), [8]) },
  P: { dims: ['N', 4], feed: (n) => new ort.Tensor('float32', iota(n * 4), [n, 4]) },
};
const iota = (count) => Float32Array.from({ length: count }, (_, i) => i);

// GraphProto: node = 1, name = 2, initializer = 5, input = 11, output = 12.
// ModelProto: ir_version = 1, graph = 7, opset_import = 8 (version = 2).
function model(name, nodes) {
  const read = new Set(nodes.flatMap((n) => n.inputs));
  const all = read.has('S') ? [node('Shape', ['X'], 'S'), ...nodes] : nodes;
  const elemType = ['Range', 'Cast'].includes(nodes.at(-1).op) ? INT64 : FLOAT;
  const graph = [
    ...all.map((n) => [1, nodeProto(n)]),
    [2, text(name)],
    ...Object.entries(constants).filter(([c]) => read.has(c)).map(([c, v]) => [5, int64Initializer(c, v)]),
    ...Object.entries(inputs).filter(([i]) => all.some((n) => n.inputs.includes(i))).map(([i, { dims }]) => [11, valueInfo(i, FLOAT, dims)]),
    [12, valueInfo(name, elemType, null)],
  ];
  return message([[1, 8], [7, graph], [8, [[2, 13]]]]);
}

mkdirSync(join(here, 'semantics'), { recursive: true });
for (const [name, nodes] of cases) {
  const path = join(here, 'semantics', `${name}.onnx`);
  writeFileSync(path, model(name, nodes));
  const session = await ort.InferenceSession.create(path);
  const lines = [];
  for (const n of lengths) {
    const feeds = Object.fromEntries(session.inputNames.map((i) => [i, inputs[i].feed(n)]));
    const out = (await session.run(feeds, [name]))[name];
    lines.push(`N = ${n}: extents [${out.dims}], data [${Array.from(out.data)}]\n`);
  }
  writeFileSync(join(here, 'semantics', `${name}.txt`), lines.join(''));
}
