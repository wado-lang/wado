// Writes test_data_set_<n>/ beside model.onnx: the first the prompt, run through
// the branch without a past, and each after it one decoding step, run through
// the branch with one against the keys and values the run before it returned.
// Each holds the tokens fed and the logits onnxruntime computes for them.
//
//   mise run loam-oracle-deps
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

const session = await ort.InferenceSession.create(join(here, 'model.onnx'));
const empty = new ort.Tensor('float32', new Float32Array(0), [1, 4, 0, 8]);

let outputs = null;
let length = 0;
for (const [n, tokens] of [prompt, ...steps.map((t) => [t])].entries()) {
  length += tokens.length;
  const feeds = {
    input_ids: ids(tokens),
    attention_mask: ones(length),
    use_cache_branch: new ort.Tensor('bool', [outputs !== null], [1]),
  };
  for (const name of session.inputNames) {
    if (name.startsWith('past_key_values.')) {
      feeds[name] = outputs === null ? empty : outputs[name.replace('past_key_values.', 'present.')];
    }
  }
  outputs = await session.run(feeds);
  const { logits } = outputs;

  const dir = join(here, `test_data_set_${n}`);
  mkdirSync(dir, { recursive: true });
  writeFileSync(join(dir, 'input_0.pb'), message(tensorProto('input_ids', INT64, feeds.input_ids.dims, raw(feeds.input_ids.data))));
  writeFileSync(join(dir, 'output_0.pb'), message(tensorProto('logits', FLOAT, logits.dims, raw(logits.data))));
  console.log(`${dir}: length ${length}, logits ${logits.dims}, first ${Array.from(logits.data.slice(0, 3))}`);
}
