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
import { FLOAT, INT32, INT64, modelProto, nodeProto, tensorProto, text, valueInfo } from '../tests/onnx_proto.mjs';

const here = process.argv[2] ?? dirname(fileURLToPath(import.meta.url));

const lengths = [3, 10];

// The int64 constants the cases compute from, each a one-element list, and
// those `Range` reads, each a scalar as ONNX defines its operands.
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
const scalars = {
  ScalarZero: 0n,
  ScalarOne: 1n,
  ScalarTwo: 2n,
  ScalarMinusOne: -1n,
  ScalarMinusTwo: -2n,
};

// Every case reads `S = Shape(X)`, and the nodes it lists after that.
const node = (op, inputs, output, attrs = {}) => ({ op, inputs, outputs: [output], attrs });
// `N` as a scalar: a scalar index gathers one element out of `S`, dropping the axis.
const length = node('Gather', ['S', 'ScalarZero'], 'N');
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
  ['RangeToLength', [length, node('Range', ['ScalarZero', 'N', 'ScalarOne'], 'RangeToLength')]],
  ['RangeByTwoToLength', [length, node('Range', ['ScalarZero', 'N', 'ScalarTwo'], 'RangeByTwoToLength')]],
  ['RangeBackFromLength', [length, node('Range', ['N', 'ScalarZero', 'ScalarMinusOne'], 'RangeBackFromLength')]],
  ['RangeBackByTwo', [length, node('Range', ['N', 'ScalarZero', 'ScalarMinusTwo'], 'RangeBackByTwo')]],
  ['CastWraps', [
    node('Cast', ['Big'], 'BigInt32', { to: INT32 }),
    node('Cast', ['BigInt32'], 'CastWraps', { to: INT64 }),
  ]],
  ['CastWrapsLength', [
    node('Mul', ['S', 'Big'], 'SBig'),
    node('Cast', ['SBig'], 'SBigInt32', { to: INT32 }),
    node('Cast', ['SBigInt32'], 'CastWrapsLength', { to: INT64 }),
  ]],
  ['ReshapeKeepSplit', [
    node('Concat', ['Zero', 'Two', 'MinusOne'], 'KeepSplitShape', { axis: 0 }),
    node('Reshape', ['P', 'KeepSplitShape'], 'ReshapeKeepSplit'),
  ]],
  ['ReshapeFlat', [node('Reshape', ['P', 'MinusOne'], 'ReshapeFlat')]],
  ['ReshapeRestFirst', [
    node('Concat', ['MinusOne', 'Two'], 'RestFirstShape', { axis: 0 }),
    node('Reshape', ['P', 'RestFirstShape'], 'ReshapeRestFirst'),
  ]],
];

function int64Initializer(name, value, dims) {
  return tensorProto(name, INT64, dims, new Uint8Array(new BigInt64Array([value]).buffer));
}

const inputs = {
  X: { dims: ['N'], feed: (n) => new ort.Tensor('float32', iota(n).map((v) => v + 10), [n]) },
  M: { dims: [8], feed: () => new ort.Tensor('float32', iota(8), [8]) },
  P: { dims: ['N', 4], feed: (n) => new ort.Tensor('float32', iota(n * 4), [n, 4]) },
};
const iota = (count) => Float32Array.from({ length: count }, (_, i) => i);

// GraphProto: node = 1, name = 2, initializer = 5, input = 11, output = 12.
function model(name, nodes) {
  const read = new Set(nodes.flatMap((n) => n.inputs));
  const all = read.has('S') ? [node('Shape', ['X'], 'S'), ...nodes] : nodes;
  const elemType = ['Range', 'Cast'].includes(nodes.at(-1).op) ? INT64 : FLOAT;
  const graph = [
    ...all.map((n) => [1, nodeProto(n)]),
    [2, text(name)],
    ...Object.entries(constants).filter(([c]) => read.has(c)).map(([c, v]) => [5, int64Initializer(c, v, [1])]),
    ...Object.entries(scalars).filter(([c]) => read.has(c)).map(([c, v]) => [5, int64Initializer(c, v, [])]),
    ...Object.entries(inputs).filter(([i]) => all.some((n) => n.inputs.includes(i))).map(([i, { dims }]) => [11, valueInfo(i, FLOAT, dims)]),
    [12, valueInfo(name, elemType, null)],
  ];
  return modelProto(graph, 13);
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
