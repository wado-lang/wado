// GPT-2 (124M) on onnxruntime: the program generate.wado times, over
// onnx/decoder_model.onnx. The first argument caps onnxruntime's threads;
// without it onnxruntime picks its own.
//
//   mise run loam-oracle-deps   # at the repository root
//   node gpt2_124m/onnxruntime.mjs [threads]

import { readFileSync } from 'node:fs';
import { createRequire } from 'node:module';

const model = new URL('../../package-loam/example/gpt2-124m/', import.meta.url);
// The onnxruntime-node that Loam's oracles pin.
const ort = createRequire(new URL('../../package-loam/package.json', import.meta.url))('onnxruntime-node');

const PROMPT = 'Hello, my name is';
const STEPS = 8;
const WARMUP = 1;
const ITERATIONS = 3;

const threads = process.argv[2] ? parseInt(process.argv[2], 10) : 0;
const session = await ort.InferenceSession.create(
  new URL('model.onnx', model).pathname,
  threads ? { intraOpNumThreads: threads, interOpNumThreads: 1 } : {},
);
const recorded = JSON.parse(readFileSync(new URL('tokenize.json', model), 'utf8')).tokenize;
const ids = recorded.find((t) => t.text === PROMPT).ids;

async function continued() {
  const all = [...ids];
  for (let s = 0; s < STEPS; s++) {
    const n = all.length;
    const { logits } = await session.run(
      {
        input_ids: new ort.Tensor('int64', BigInt64Array.from(all, BigInt), [1, n]),
        attention_mask: new ort.Tensor('int64', new BigInt64Array(n).fill(1n), [1, n]),
      },
      ['logits'],
    );
    const vocab = logits.dims[2];
    const last = logits.data.subarray((n - 1) * vocab, n * vocab);
    let best = 0;
    for (let v = 1; v < vocab; v++) if (last[v] > last[best]) best = v;
    all.push(best);
  }
  return all.slice(ids.length);
}

/** The throughput line `core:benchmark` prints, which `logs.ts` reads. */
function printThroughput(label, workPerIter, n, elapsedNs, unit) {
  const secs = elapsedNs / 1e9;
  const rate = secs > 0 ? (workPerIter * n) / secs : 0;
  const perMs = elapsedNs / n / 1e6;
  let rbuf;
  if (rate >= 1e9) rbuf = `${(rate / 1e9).toFixed(2)} G ${unit}/s`;
  else if (rate >= 1e6) rbuf = `${(rate / 1e6).toFixed(2)} M ${unit}/s`;
  else if (rate >= 1e3) rbuf = `${(rate / 1e3).toFixed(2)} k ${unit}/s`;
  else rbuf = `${rate.toFixed(2)} ${unit}/s`;
  console.log(`${label}: ${rbuf}   (${perMs.toFixed(3)} ms/iter, ${n} iter)`);
}

let generated = [];
for (let i = 0; i < WARMUP; i++) generated = await continued();
const started = process.hrtime.bigint();
for (let i = 0; i < ITERATIONS; i++) generated = await continued();
printThroughput('generate', STEPS, ITERATIONS, Number(process.hrtime.bigint() - started), 'tokens');
console.log(`generated = [${generated.join(', ')}]`);
