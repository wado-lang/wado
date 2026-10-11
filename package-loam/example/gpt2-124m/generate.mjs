// Writes generate.json beside model.onnx: the tokens onnxruntime picks
// greedily after each prompt, from the prompt ids tokenize.json records, and
// how long each of its steps took on the CPU.
//
//   mise run loam-oracle-deps
//   node generate.mjs
import { readFileSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import * as ort from 'onnxruntime-node';

const here = dirname(fileURLToPath(import.meta.url));
const recorded = JSON.parse(readFileSync(join(here, 'tokenize.json'), 'utf8')).tokenize;
const PROMPTS = ['Hello, my name is'];
const STEPS = 8;

const session = await ort.InferenceSession.create(join(here, 'model.onnx'));

/** The next token onnxruntime's logits rank first, and its lead over the second. */
async function next(ids) {
  const n = ids.length;
  const { logits } = await session.run(
    {
      input_ids: new ort.Tensor('int64', BigInt64Array.from(ids, BigInt), [1, n]),
      attention_mask: new ort.Tensor('int64', new BigInt64Array(n).fill(1n), [1, n]),
    },
    ['logits'],
  );
  const vocab = logits.dims[2];
  const last = logits.data.subarray((n - 1) * vocab, n * vocab);
  let best = 0;
  for (let v = 1; v < vocab; v++) if (last[v] > last[best]) best = v;
  let second = -Infinity;
  for (let v = 0; v < vocab; v++) if (v !== best && last[v] > second) second = last[v];
  return [best, last[best] - second];
}

let lead = Infinity;
const generate = [];
for (const prompt of PROMPTS) {
  const { ids } = recorded.find((t) => t.text === prompt);
  const all = [...ids];
  const started = performance.now();
  for (let s = 0; s < STEPS; s++) {
    const [token, margin] = await next(all);
    all.push(token);
    lead = Math.min(lead, margin);
  }
  const ms_per_token = (performance.now() - started) / STEPS;
  generate.push({ prompt, ids, generated: all.slice(ids.length), ms_per_token });
}

writeFileSync(join(here, 'generate.json'), `${JSON.stringify({ generate }, null, 2)}\n`);
console.log(`smallest lead of a greedy pick over the runner-up: ${lead}`);
for (const g of generate) console.log(`${JSON.stringify(g.prompt)} -> ${g.generated} (${g.ms_per_token.toFixed(1)} ms a token)`);
