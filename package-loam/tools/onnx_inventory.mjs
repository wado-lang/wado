// What a model asks of Loam, read from its graph alone: each operator with the
// attribute values and the input and output counts its nodes use, the graph's
// inputs and outputs, and its weights by element type and by the file holding
// them. `loam dump` stops at the first node it refuses, so this is how the rest
// are seen before the build reaches them.
//
//   mise run loam-onnx-inventory <model.onnx>
import { readFileSync } from 'node:fs';
import onnxProto from 'onnx-proto';

const { onnx } = onnxProto;

if (process.argv.length !== 3) {
  console.error('usage: onnx_inventory.mjs <model.onnx>');
  process.exit(2);
}
const model = onnx.ModelProto.decode(readFileSync(process.argv[2]));

const typeName = (n) =>
  Object.entries(onnx.TensorProto.DataType).find(([, v]) => v === n)?.[0] ?? String(n);
const typed = (v) => {
  const t = v.type.tensorType;
  const dims = t.shape?.dim?.map((d) => d.dimParam || String(d.dimValue)).join(',');
  return `${v.name}: ${typeName(t.elemType)}[${dims}]`;
};
const attrValue = (a) => {
  if (a.g) return 'graph';
  if (a.t) return `tensor<${typeName(a.t.dataType)}>[${a.t.dims}]`;
  if (a.s?.length) return Buffer.from(a.s).toString();
  if (a.ints?.length) return `[${a.ints.join(',')}]`;
  return String(a.f || a.i);
};

// An attribute's values are capped, since a `Constant`'s are as many as its nodes.
const VALUES_SHOWN = 6;
const ops = new Map();
const walk = (graph) => {
  for (const node of graph.node) {
    const key = `${node.domain || 'ai.onnx'}::${node.opType}`;
    const op = ops.get(key) ?? { count: 0, attrs: new Map(), ins: new Set(), outs: new Set() };
    ops.set(key, op);
    op.count++;
    op.ins.add(node.input.length);
    op.outs.add(node.output.length);
    for (const a of node.attribute) {
      const values = op.attrs.get(a.name) ?? new Set();
      op.attrs.set(a.name, values);
      if (values.size < VALUES_SHOWN) values.add(attrValue(a));
      if (a.g) walk(a.g);
    }
  }
};
walk(model.graph);

console.log('opsets', model.opsetImport.map((o) => `${o.domain || 'ai.onnx'}=${o.version}`).join(' '));
console.log('functions', model.functions.length);
console.log('\noperators');
for (const [key, op] of [...ops].sort()) {
  console.log(`  ${key} x${op.count} inputs=${[...op.ins]} outputs=${[...op.outs]}`);
  for (const [name, values] of op.attrs) console.log(`    ${name}: ${[...values].join(' | ')}`);
}

const initializers = new Set(model.graph.initializer.map((t) => t.name));
console.log('\ninputs');
for (const v of model.graph.input) if (!initializers.has(v.name)) console.log(`  ${typed(v)}`);
console.log('outputs');
for (const v of model.graph.output) console.log(`  ${typed(v)}`);

const tally = (map, key, bytes) => {
  const entry = map.get(key) ?? { tensors: 0, bytes: 0 };
  map.set(key, entry);
  entry.tensors++;
  entry.bytes += bytes;
};
const byType = new Map();
const byFile = new Map();
const weights = [];
for (const t of model.graph.initializer) {
  const external = Object.fromEntries(t.externalData.map((e) => [e.key, e.value]));
  const bytes = external.length ? Number(external.length) : t.rawData.length;
  tally(byType, typeName(t.dataType), bytes);
  tally(byFile, external.location ?? '(in the graph)', bytes);
  weights.push({ name: t.name, type: typeName(t.dataType), dims: t.dims.join(','), bytes });
}
const LARGEST_SHOWN = 12;
for (const [title, map] of [['weights by element type', byType], ['weights by file', byFile]]) {
  console.log(`\n${title}`);
  for (const [key, e] of map) console.log(`  ${key}: ${e.tensors} tensors, ${e.bytes} bytes`);
}
console.log('\nlargest weights');
for (const w of weights.sort((a, b) => b.bytes - a.bytes).slice(0, LARGEST_SHOWN)) {
  console.log(`  ${w.name}: ${w.type}[${w.dims}] ${w.bytes} bytes`);
}
