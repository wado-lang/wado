// Writes generate.json beside model.onnx: the ids Hugging Face's tokenizer gives
// each text below, and the tokens onnxruntime picks greedily after each prompt.
//
//   npm install onnxruntime-node@1.30.0 @huggingface/tokenizers@0.2.0
//   node generate.mjs <this directory> <a directory holding tokenizer.json>
import { readFileSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import * as ort from 'onnxruntime-node';
import { Tokenizer } from '@huggingface/tokenizers';

const here = process.argv[2] ?? dirname(fileURLToPath(import.meta.url));
const tokenizerDir = process.argv[3] ?? here;
const readJson = (name) => JSON.parse(readFileSync(join(tokenizerDir, name), 'utf8'));
const tokenizer = new Tokenizer(readJson('tokenizer.json'), readJson('tokenizer_config.json'));
// Byte-level decoding is exact; the cleanup would drop the space in " ...".
const decode = (ids) => tokenizer.decode(ids, { clean_up_tokenization_spaces: false });

const TEXTS = [
  'Hello world',
  "It's what they've said: we'll see, I'd guess, you're right, I'm here.",
  "Don't SHOUT'S",
  '  two leading spaces, and three   inside',
  'trailing spaces   ',
  'tab\there\nnewline\n\nblank line',
  ' \n mixed \t\n whitespace',
  'digits 12345 and 3.14 and x2',
  'punctuation!!! ...?? (braces) [brackets] {curly} <angle> #hash @at',
  'café naïve Ωmega',
  '日本語のテキスト',
  'emoji 😀👍🏽 done',
  'end<|endoftext|>start',
];
const PROMPTS = ['Hello, my name is', 'The quick brown fox'];
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

const tokenize = TEXTS.map((text) => {
  const { ids } = tokenizer.encode(text, { add_special_tokens: false });
  if (decode(ids) !== text) throw new Error(`no round trip: ${JSON.stringify(text)}`);
  return { text, ids };
});

let lead = Infinity;
const generate = [];
for (const prompt of PROMPTS) {
  const { ids } = tokenizer.encode(prompt, { add_special_tokens: false });
  const all = [...ids];
  for (let s = 0; s < STEPS; s++) {
    const [token, margin] = await next(all);
    all.push(token);
    lead = Math.min(lead, margin);
  }
  const generated = all.slice(ids.length);
  generate.push({ prompt, ids, generated, text: decode(all) });
}

writeFileSync(join(here, 'generate.json'), `${JSON.stringify({ tokenize, generate }, null, 2)}\n`);
console.log(`smallest lead of a greedy pick over the runner-up: ${lead}`);
for (const g of generate) console.log(`${g.ids.length} prompt ids -> ${JSON.stringify(g.text)}`);
