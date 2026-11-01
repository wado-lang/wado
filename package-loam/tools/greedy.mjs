// What each example's generate.mjs writes to generate.json: the tokens
// onnxruntime picks greedily after each prompt, from the prompt ids
// tokenize.json records, and how long a token after the prompt's first took on
// the CPU and on WebGPU. It runs the model as Loam does, feeding each step only
// the new token and the attention cache of the rest. Both providers must pick
// the same tokens.
import { readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import * as ort from 'onnxruntime-node';

const STEPS = 8;

/**
 * Writes `dir`/generate.json for `model`, continuing each of `prompts`.
 * `emptyPast` is the shape of a layer's cache before the first token, and
 * `feeds(seen)` any input beyond the ids, the mask and the cache.
 */
export async function writeGenerated({ dir, model, prompts, layers, emptyPast, feeds = () => ({}) }) {
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

  /** The next token after `fresh`, given the caches of the `seen` ids before it. */
  async function next(session, fresh, caches, seen) {
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
    return [best, last[best] - second, out];
  }

  /** Each prompt's tokens on `provider`, the time a token after the first, and the smallest lead. */
  async function generate(provider) {
    // On WebGPU the cache stays on the device between steps, and only the
    // logits come back.
    const preferredOutputLocation = provider === 'webgpu' ? { logits: 'cpu' } : undefined;
    for (let l = 0; preferredOutputLocation && l < layers; l++) {
      for (const kind of ['key', 'value']) preferredOutputLocation[`present.${l}.${kind}`] = 'gpu-buffer';
    }
    const session = await ort.InferenceSession.create(model, { executionProviders: [provider], preferredOutputLocation });
    let lead = Infinity;
    const timed = [];
    for (const prompt of prompts) {
      const { ids } = recorded.find((t) => t.text === prompt);
      const generated = [];
      let caches = {};
      let fresh = ids;
      let seen = 0;
      let started;
      for (let s = 0; s < STEPS; s++) {
        const [token, margin, out] = await next(session, fresh, caches, seen);
        if (s === 0) started = performance.now();
        seen += fresh.length;
        caches = out;
        generated.push(token);
        fresh = [token];
        lead = Math.min(lead, margin);
      }
      timed.push({ prompt, ids, generated, ms_per_token: (performance.now() - started) / (STEPS - 1) });
    }
    return { prompts: timed, lead };
  }
}
