// Writes test_data_set_<n>/ beside model.onnx, one per decoding step: the token
// fed and the logits onnxruntime computes for it. The prompt runs through the
// decoder without a past, ../model.onnx, and each step takes the keys and values
// the run before it returned, so the cache grows by one position per step.
//
//   npm install onnxruntime-node@1.30.0
//   node oracle.mjs <this directory>
import { writeFileSync, mkdirSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import * as ort from 'onnxruntime-node';
import { FLOAT, INT64, message, tensorProto } from '../../../onnx_proto.mjs';

const here = process.argv[2] ?? dirname(fileURLToPath(import.meta.url));
const prompt = [1n, 5n, 42n, 7n];
const steps = [11n, 13n];

const ids = (values) => new ort.Tensor('int64', BigInt64Array.from(values), [1, values.length]);
const ones = (n) => new ort.Tensor('int64', new BigInt64Array(n).fill(1n), [1, n]);
const raw = (typed) => new Uint8Array(typed.buffer, typed.byteOffset, typed.byteLength);

const decoder = await ort.InferenceSession.create(join(here, '..', 'model.onnx'));
const step = await ort.InferenceSession.create(join(here, 'model.onnx'));

let outputs = await decoder.run({ input_ids: ids(prompt), attention_mask: ones(prompt.length) });
let length = prompt.length;
for (const [n, token] of steps.entries()) {
  length += 1;
  const feeds = { input_ids: ids([token]), attention_mask: ones(length) };
  for (const name of step.inputNames) {
    if (name.startsWith('past_key_values.')) {
      feeds[name] = outputs[name.replace('past_key_values.', 'present.')];
    }
  }
  outputs = await step.run(feeds);
  const { logits } = outputs;

  const dir = join(here, `test_data_set_${n}`);
  mkdirSync(dir, { recursive: true });
  writeFileSync(join(dir, 'input_0.pb'), message(tensorProto('input_ids', INT64, feeds.input_ids.dims, raw(feeds.input_ids.data))));
  writeFileSync(join(dir, 'output_0.pb'), message(tensorProto('logits', FLOAT, logits.dims, raw(logits.data))));
  console.log(`${dir}: past ${length - 1}, logits ${logits.dims}, first ${Array.from(logits.data.slice(0, 3))}`);
}
