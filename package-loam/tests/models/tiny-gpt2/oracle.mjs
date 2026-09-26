// Writes test_data_set_<n>/ beside model.onnx, one per sequence below: the
// inputs and the logits onnxruntime computes from them, each as one bare
// TensorProto. The sequences differ in length, so one module compiled with the
// length left symbolic must reproduce both.
//
//   npm install onnxruntime-node@1.30.0
//   node oracle.mjs <this directory>
import { writeFileSync, mkdirSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import * as ort from 'onnxruntime-node';

const here = process.argv[2] ?? dirname(fileURLToPath(import.meta.url));
const sequences = [
  [1n, 5n, 42n, 7n],
  [3n, 14n, 15n, 92n, 65n, 35n, 89n],
];

function varint(n) {
  const out = [];
  let v = BigInt(n);
  while (v >= 0x80n) {
    out.push(Number(v & 0x7fn) | 0x80);
    v >>= 7n;
  }
  out.push(Number(v));
  return out;
}

// TensorProto fields: dims = 1, data_type = 2, name = 8, raw_data = 9.
function tensorProto(name, dataType, dims, raw) {
  const bytes = [];
  for (const d of dims) bytes.push(0x08, ...varint(d));
  bytes.push(0x10, ...varint(dataType));
  const nameBytes = Buffer.from(name, 'utf8');
  bytes.push(0x42, ...varint(nameBytes.length), ...nameBytes);
  bytes.push(0x4a, ...varint(raw.length), ...raw);
  return Buffer.from(bytes);
}

const INT64 = 7;
const FLOAT = 1;
const session = await ort.InferenceSession.create(join(here, 'model.onnx'));
const raw = (typed) => new Uint8Array(typed.buffer, typed.byteOffset, typed.byteLength);
for (const [n, ids] of sequences.entries()) {
  const mask = ids.map(() => 1n);
  const inputs = {
    input_ids: new ort.Tensor('int64', BigInt64Array.from(ids), [1, ids.length]),
    attention_mask: new ort.Tensor('int64', BigInt64Array.from(mask), [1, mask.length]),
  };
  const { logits } = await session.run(inputs, ['logits']);

  const dir = join(here, `test_data_set_${n}`);
  mkdirSync(dir, { recursive: true });
  writeFileSync(join(dir, 'input_0.pb'), tensorProto('input_ids', INT64, inputs.input_ids.dims, raw(inputs.input_ids.data)));
  writeFileSync(join(dir, 'input_1.pb'), tensorProto('attention_mask', INT64, inputs.attention_mask.dims, raw(inputs.attention_mask.data)));
  writeFileSync(join(dir, 'output_0.pb'), tensorProto('logits', FLOAT, logits.dims, raw(logits.data)));
  console.log(`${dir}: logits ${logits.dims}, first ${Array.from(logits.data.slice(0, 3))}`);
}
