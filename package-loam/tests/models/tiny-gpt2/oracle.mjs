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
import { FLOAT, INT64, message, tensorProto } from '../../onnx_proto.mjs';

const here = process.argv[2] ?? dirname(fileURLToPath(import.meta.url));
const sequences = [
  [1n, 5n, 42n, 7n],
  [3n, 14n, 15n, 92n, 65n, 35n, 89n],
];

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
  writeFileSync(join(dir, 'input_0.pb'), message(tensorProto('input_ids', INT64, inputs.input_ids.dims, raw(inputs.input_ids.data))));
  writeFileSync(join(dir, 'input_1.pb'), message(tensorProto('attention_mask', INT64, inputs.attention_mask.dims, raw(inputs.attention_mask.data))));
  writeFileSync(join(dir, 'output_0.pb'), message(tensorProto('logits', FLOAT, logits.dims, raw(logits.data))));
  console.log(`${dir}: logits ${logits.dims}, first ${Array.from(logits.data.slice(0, 3))}`);
}
