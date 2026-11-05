// An example's model on onnxruntime-node's WebGPU provider: the program
// generate.wado times, timed as harness.hpp times the C++ arms. The argument
// is the example's directory, whose decoder.mjs says how to feed the model and
// whose generate.json records the prompt's ids.
//
//   mise run gpt2-124m   # runs it
import { readFileSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { pathToFileURL } from 'node:url';
import { STEPS, continued, opened } from '../tools/greedy.mjs';

const PROMPT = 'Hello, my name is';
const WARMUP = 1;
const ITERATIONS = 3;

const dir = resolve(process.argv[2]);
const { decoder } = await import(pathToFileURL(join(dir, 'decoder.mjs')).href);
const { generate } = JSON.parse(readFileSync(join(dir, 'generate.json'), 'utf8'));
const { ids } = generate.find((g) => g.prompt === PROMPT);
const session = await opened(decoder, 'webgpu');

for (let i = 0; i < WARMUP; i++) await continued(session, decoder, ids);
const started = performance.now();
let generated;
for (let i = 0; i < ITERATIONS; i++) ({ generated } = await continued(session, decoder, ids));
const elapsed = performance.now() - started;
const rate = (STEPS * ITERATIONS) / (elapsed / 1000);
console.log(`generate: ${rate.toFixed(2)} tokens/s   (${(elapsed / ITERATIONS).toFixed(3)} ms/iter, ${ITERATIONS} iter)`);
console.log(`generated = [${generated.join(', ')}]`);
