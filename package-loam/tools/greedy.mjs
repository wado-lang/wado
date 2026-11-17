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

export const STEPS = 32;

// Where the global exists, onnxruntime holds float16 in a Float16Array, which
// its native side reads as empty when one is fed back on the CPU. Without the
// global it holds the bits. It looks once, at the first tensor.
const Float16Array = globalThis.Float16Array;
delete globalThis.Float16Array;

/**
 * Writes `dir`/`name` for `decoder`, continuing each of `prompts`, and into
 * `dir`/`logits`, where given, the logits the CPU picks each prompt's first
 * token from: little-endian f32s, a prompt after another. A prompt's tokens
 * stop at the first of `ends`, the tokens that end a reply: the picks after
 * one are noise two providers part on.
 * A decoder is `{ model, layers, emptyPast, vocab, half, feeds }`: `emptyPast`
 * is the shape of a layer's cache before the first token, `vocab` the logits'
 * last extent, `half` says the cache and the logits are float16, and
 * `feeds(seen)` any input beyond the ids, the mask and the cache.
 */
export async function writeGenerated({ dir, prompts, ends = [], decoder, name = 'generate.json', logits }) {
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

  writeFileSync(join(dir, name), `${JSON.stringify({ generate: generated }, null, 2)}\n`);
  if (logits) writeFileSync(join(dir, logits), Buffer.concat(cpu.firsts.map((f) => Buffer.from(f.buffer))));
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
    const firsts = [];
    for (const prompt of prompts) {
      const { ids } = recorded.find((t) => t.text === prompt);
      const c = await continued(session, decoder, ids, ends);
      if (c.generated.length < 2) throw new Error(`the reply to ${JSON.stringify(prompt)} ends at its first token`);
      lead = Math.min(lead, c.lead);
      firsts.push(c.first);
      timed.push({ prompt, ids, generated: c.generated, ms_per_token: c.ms_after_first / (c.generated.length - 1) });
    }
    return { prompts: timed, lead, firsts };
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
 * The STEPS tokens greedy decoding picks after `ids` from an empty cache, or
 * those up to the first of `ends`, the smallest lead of a pick over the
 * runner-up, the logits the first is picked from, and the milliseconds the
 * steps after the first took.
 */
export async function continued(session, { layers, emptyPast, half = false, feeds = () => ({}) }, ids, ends = []) {
  const empty = half ? new ort.Tensor('float16', new Uint16Array(0), emptyPast) : new ort.Tensor('float32', new Float32Array(0), emptyPast);
  const generated = [];
  let lead = Infinity;
  let caches = {};
  let fresh = ids;
  let seen = 0;
  let started;
  let first;
  for (let s = 0; s < STEPS && !ends.includes(generated.at(-1)); s++) {
    const total = seen + fresh.length;
    const inputs = {
      input_ids: new ort.Tensor('int64', BigInt64Array.from(fresh, BigInt), [1, fresh.length]),
      attention_mask: new ort.Tensor('int64', new BigInt64Array(total).fill(1n), [1, total]),
      ...feeds(seen),
    };
    for (let l = 0; l < layers; l++) {
      for (const kind of ['key', 'value']) {
        inputs[`past_key_values.${l}.${kind}`] = caches[`present.${l}.${kind}`] ?? empty;
      }
    }
    const out = await session.run(inputs);
    const vocab = out.logits.dims[2];
    const last = values(out.logits.data).subarray((fresh.length - 1) * vocab, fresh.length * vocab);
    let best = 0;
    for (let v = 1; v < vocab; v++) if (last[v] > last[best]) best = v;
    let second = -Infinity;
    for (let v = 0; v < vocab; v++) if (v !== best && last[v] > second) second = last[v];
    if (s === 0) {
      first = Float32Array.from(last);
      started = performance.now();
    }
    seen = total;
    released(caches);
    caches = out;
    generated.push(best);
    fresh = [best];
    lead = Math.min(lead, last[best] - second);
  }
  released(caches);
  return { generated, lead, first, ms_after_first: performance.now() - started };
}

/** A tensor's data as numbers, which float16's bits are not. */
function values(data) {
  return data instanceof Uint16Array ? new Float16Array(data.buffer, data.byteOffset, data.length) : data;
}

/** Frees the caches a step returned on the device, which only `dispose` does. */
function released(caches) {
  for (const t of Object.values(caches)) {
    if (t.location === 'gpu-buffer') t.dispose();
  }
}
