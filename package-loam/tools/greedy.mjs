// A decoder continued greedily on onnxruntime, as Loam runs one: each step
// feeds only the new tokens and the attention cache of the rest. Each
// example's decoder.mjs says how to feed its model.
//
// `writeGenerated` writes an example's generate.json: the tokens onnxruntime
// picks after each prompt, from the prompt ids tokenize.json records, and how
// long a token after the prompt's first took on the CPU and on WebGPU. Both
// providers must pick the same tokens. `../benchmark/onnxruntime_webgpu.mjs`
// times the same decoding.
import { readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import * as ort from 'onnxruntime-node';

export const STEPS = 8;

/**
 * Writes `dir`/generate.json for `decoder`, continuing each of `prompts`.
 * A decoder is `{ model, layers, emptyPast, feeds }`: `emptyPast` is the shape
 * of a layer's cache before the first token, and `feeds(seen)` any input
 * beyond the ids, the mask and the cache.
 */
export async function writeGenerated({ dir, prompts, decoder }) {
  const recorded = JSON.parse(readFileSync(join(dir, 'tokenize.json'), 'utf8')).tokenize;
  const cpu = await generate('cpu');
  const webgpu = await generate('webgpu');
  const generated = cpu.prompts.map((c, i) => {
    const w = webgpu.prompts[i];
    if (`${w.generated}` !== `${c.generated}`) {
      throw new Error(`WebGPU picks ${w.generated} after ${JSON.stringify(c.prompt)}, the CPU ${c.generated}`);
    }
    return { ...c, webgpu_ms_per_token: w.ms_per_token };
  });

  writeFileSync(join(dir, 'generate.json'), `${JSON.stringify({ generate: generated }, null, 2)}\n`);
  console.log(`smallest lead of a greedy pick over the runner-up: ${cpu.lead}`);
  for (const g of generated) {
    console.log(
      `${JSON.stringify(g.prompt)} -> ${g.generated} (${g.ms_per_token.toFixed(1)} ms a token on the CPU, ${g.webgpu_ms_per_token.toFixed(1)} on WebGPU)`,
    );
  }

  /** Each prompt's tokens on `provider`, the time a token after the first, and the smallest lead. */
  async function generate(provider) {
    const session = await opened(decoder, provider);
    let lead = Infinity;
    const timed = [];
    for (const prompt of prompts) {
      const { ids } = recorded.find((t) => t.text === prompt);
      const c = await continued(session, decoder, ids);
      lead = Math.min(lead, c.lead);
      timed.push({ prompt, ids, generated: c.generated, ms_per_token: c.ms_after_first / (STEPS - 1) });
    }
    return { prompts: timed, lead };
  }
}

/**
 * A session of `decoder`'s model on `provider`. On WebGPU the cache stays on
 * the device between steps, and only the logits come back.
 */
export async function opened({ model, layers }, provider) {
  const preferredOutputLocation = provider === 'webgpu' ? { logits: 'cpu' } : undefined;
  for (let l = 0; preferredOutputLocation && l < layers; l++) {
    for (const kind of ['key', 'value']) preferredOutputLocation[`present.${l}.${kind}`] = 'gpu-buffer';
  }
  return ort.InferenceSession.create(model, { executionProviders: [provider], preferredOutputLocation });
}

/**
 * The STEPS tokens greedy decoding picks after `ids` from an empty cache, the
 * smallest lead of a pick over the runner-up, and the milliseconds the steps
 * after the first took.
 */
export async function continued(session, { layers, emptyPast, feeds = () => ({}) }, ids) {
  const generated = [];
  let lead = Infinity;
  let caches = {};
  let fresh = ids;
  let seen = 0;
  let started;
  for (let s = 0; s < STEPS; s++) {
    const total = seen + fresh.length;
    const inputs = {
      input_ids: new ort.Tensor('int64', BigInt64Array.from(fresh, BigInt), [1, fresh.length]),
      attention_mask: new ort.Tensor('int64', new BigInt64Array(total).fill(1n), [1, total]),
      ...feeds(seen),
    };
    for (let l = 0; l < layers; l++) {
      for (const kind of ['key', 'value']) {
        inputs[`past_key_values.${l}.${kind}`] = caches[`present.${l}.${kind}`] ?? new ort.Tensor('float32', new Float32Array(0), emptyPast);
      }
    }
    const out = await session.run(inputs);
    const vocab = out.logits.dims[2];
    const last = out.logits.data.subarray((fresh.length - 1) * vocab, fresh.length * vocab);
    let best = 0;
    for (let v = 1; v < vocab; v++) if (last[v] > last[best]) best = v;
    let second = -Infinity;
    for (let v = 0; v < vocab; v++) if (v !== best && last[v] > second) second = last[v];
    if (s === 0) started = performance.now();
    seen = total;
    released(caches);
    caches = out;
    generated.push(best);
    fresh = [best];
    lead = Math.min(lead, last[best] - second);
  }
  released(caches);
  return { generated, lead, ms_after_first: performance.now() - started };
}

/** Frees the caches a step returned on the device, which only `dispose` does. */
function released(caches) {
  for (const t of Object.values(caches)) {
    if (t.location === 'gpu-buffer') t.dispose();
  }
}
