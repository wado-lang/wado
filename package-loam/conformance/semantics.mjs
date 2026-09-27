// Writes semantics/<case>.onnx beside this script, one model per case of
// ONNX's shape arithmetic over a length only the run knows, `N` = the length of
// `X`. Then runs each through onnxruntime at each of `lengths` and prints what
// it computed, which semantics_test.wado states as the expectation. `M` holds
// 8 elements, so the second length runs past it.
//
//   npm install onnxruntime-node@1.30.0
//   node semantics.mjs <this directory>
import { mkdirSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import * as ort from 'onnxruntime-node';

const here = process.argv[2] ?? dirname(fileURLToPath(import.meta.url));

const lengths = [3, 10];
const FLOAT = 1;
const INT64 = 7;
const INT32 = 6;

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

// A protobuf message as its fields: [number, value], where a value is a
// varint (bigint or number), a Buffer (length-delimited), or an array of
// fields (a nested message).
function varint(n) {
  const out = [];
  let v = BigInt.asUintN(64, BigInt(n));
  while (v >= 0x80n) {
    out.push(Number(v & 0x7fn) | 0x80);
    v >>= 7n;
  }
  out.push(Number(v));
  return out;
}

function message(fields) {
  const bytes = [];
  for (const [number, value] of fields) {
    if (Array.isArray(value) || Buffer.isBuffer(value)) {
      const body = Buffer.isBuffer(value) ? value : message(value);
      bytes.push(...varint((number << 3) | 2), ...varint(body.length), ...body);
    } else {
      bytes.push(...varint(number << 3), ...varint(value));
    }
  }
  return Buffer.from(bytes);
}

const text = (s) => Buffer.from(s, 'utf8');

// ValueInfoProto: name = 1, type = 2; TypeProto.tensor_type = 1;
// Tensor: elem_type = 1, shape = 2; TensorShapeProto.dim = 1;
// Dimension: dim_value = 1, dim_param = 2.
function valueInfo(name, elemType, dims) {
  const dim = (d) => [1, typeof d === 'string' ? [[2, text(d)]] : [[1, d]]];
  const shape = dims === null ? [] : [[2, dims.map(dim)]];
  return [[1, text(name)], [2, [[1, [[1, elemType], ...shape]]]]];
}

// TensorProto: dims = 1, data_type = 2, name = 8, raw_data = 9.
function int64Initializer(name, value) {
  const raw = Buffer.from(new BigInt64Array([value]).buffer);
  return [[1, 1], [2, INT64], [8, text(name)], [9, raw]];
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
  for (const n of lengths) {
    const feeds = Object.fromEntries(session.inputNames.map((i) => [i, inputs[i].feed(n)]));
    try {
      const out = (await session.run(feeds, [name]))[name];
      console.log(`N = ${n}, ${name}: extents [${out.dims}], data [${Array.from(out.data)}]`);
    } catch (e) {
      console.log(`N = ${n}, ${name}: ${e.message}`);
    }
  }
}
