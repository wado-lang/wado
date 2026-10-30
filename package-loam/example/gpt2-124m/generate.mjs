// Writes generate.json beside model.onnx: the tokens onnxruntime picks
// greedily after each prompt, from the prompt ids tokenize.json records, and
// how long each of its steps took on the CPU. It runs model.onnx as Loam does,
// feeding each step only the new token and the attention cache of the rest.
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
const LAYERS = 12;
const HEADS = 12;
const DIM = 64;

const session = await ort.InferenceSession.create(join(here, 'model.onnx'));

/** The next token after `fresh`, given the caches of the `seen` ids before it. */
async function next(fresh, caches, seen) {
  const total = seen + fresh.length;
  const feeds = {
    input_ids: new ort.Tensor('int64', BigInt64Array.from(fresh, BigInt), [1, fresh.length]),
    attention_mask: new ort.Tensor('int64', new BigInt64Array(total).fill(1n), [1, total]),
    use_cache_branch: new ort.Tensor('bool', [seen > 0], [1]),
  };
  for (let l = 0; l < LAYERS; l++) {
    for (const kind of ['key', 'value']) {
      feeds[`past_key_values.${l}.${kind}`] = caches[`present.${l}.${kind}`] ?? new ort.Tensor('float32', new Float32Array(0), [1, HEADS, 0, DIM]);
    }
  }
  const out = await session.run(feeds);
  const vocab = out.logits.dims[2];
  const last = out.logits.data.subarray((fresh.length - 1) * vocab, fresh.length * vocab);
  let best = 0;
  for (let v = 1; v < vocab; v++) if (last[v] > last[best]) best = v;
  let second = -Infinity;
  for (let v = 0; v < vocab; v++) if (v !== best && last[v] > second) second = last[v];
  return [best, last[best] - second, out];
}

let lead = Infinity;
const generate = [];
for (const prompt of PROMPTS) {
  const { ids } = recorded.find((t) => t.text === prompt);
  const generated = [];
  let caches = {};
  let fresh = ids;
  let seen = 0;
  const started = performance.now();
  for (let s = 0; s < STEPS; s++) {
    const [token, margin, out] = await next(fresh, caches, seen);
    seen += fresh.length;
    caches = out;
    generated.push(token);
    fresh = [token];
    lead = Math.min(lead, margin);
  }
  const ms_per_token = (performance.now() - started) / STEPS;
  generate.push({ prompt, ids, generated, ms_per_token });
}

writeFileSync(join(here, 'generate.json'), `${JSON.stringify({ generate }, null, 2)}\n`);
console.log(`smallest lead of a greedy pick over the runner-up: ${lead}`);
for (const g of generate) console.log(`${JSON.stringify(g.prompt)} -> ${g.generated} (${g.ms_per_token.toFixed(1)} ms a token)`);
