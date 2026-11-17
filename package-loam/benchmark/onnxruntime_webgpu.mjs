// An example's model on onnxruntime-node's WebGPU provider: the program
// generate.wado times, timed as harness.hpp times the C++ arms. The argument
// is the example's directory, whose decoder.mjs says how to feed the model and
// whose generate.json records the prompt's ids. A second argument names
// another export of decoder.mjs than `decoder`, such as `decoderQ4f16`.
//
// Each step reads back only the next token, which the graph picks
// (`tools/picking.mjs`), and keeps the cache on the device.
//
//   mise run gpt2-124m   # runs it
import { readFileSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { pathToFileURL } from 'node:url';
import * as ort from 'onnxruntime-node';
import { STEPS, fed, released } from '../tools/greedy.mjs';
import { NEXT, picking } from '../tools/picking.mjs';

const PROMPT = 'Hello, my name is';
const WARMUP = 1;
const ITERATIONS = 3;

const dir = resolve(process.argv[2]);
const decoder = (await import(pathToFileURL(join(dir, 'decoder.mjs')).href))[process.argv[3] ?? 'decoder'];
const { generate } = JSON.parse(readFileSync(join(dir, 'generate.json'), 'utf8'));
const { ids } = generate.find((g) => g.prompt === PROMPT);
const session = await opened(decoder);

for (let i = 0; i < WARMUP; i++) await continued(session, decoder, ids);
const started = performance.now();
let generated;
for (let i = 0; i < ITERATIONS; i++) generated = await continued(session, decoder, ids);
const elapsed = performance.now() - started;
const rate = (STEPS * ITERATIONS) / (elapsed / 1000);
console.log(`generate: ${rate.toFixed(2)} tokens/s   (${(elapsed / ITERATIONS).toFixed(3)} ms/iter, ${ITERATIONS} iter)`);
console.log(`generated = [${generated.join(', ')}]`);

/**
 * A session of `decoder`'s model with the pick appended. The node binding
 * misplaces outputs when a run names those it returns, so each run returns
 * all of them, the logits left on the device.
 */
function opened({ model, layers, vocab }) {
  const preferredOutputLocation = { logits: 'gpu-buffer' };
  for (let l = 0; l < layers; l++) {
    for (const kind of ['key', 'value']) preferredOutputLocation[`present.${l}.${kind}`] = 'gpu-buffer';
  }
  preferredOutputLocation[NEXT] = 'cpu';
  return ort.InferenceSession.create(picking(model, vocab, 'blocks'), { executionProviders: ['webgpu'], preferredOutputLocation });
}

/** The STEPS tokens greedy decoding picks after `ids` from an empty cache. */
async function continued(session, decoder, ids) {
  const generated = [];
  let caches = {};
  let fresh = ids;
  let seen = 0;
  for (let s = 0; s < STEPS; s++) {
    const out = await session.run(fed(decoder, fresh, seen, caches));
    released(caches);
    caches = out;
    seen += fresh.length;
    const next = Number(out[NEXT].data[0]);
    generated.push(next);
    fresh = [next];
  }
  released(caches);
  return generated;
}
