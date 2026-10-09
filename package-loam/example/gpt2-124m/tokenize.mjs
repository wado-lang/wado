// Writes tokenize.json beside tokenizer.json: the ids Hugging Face's tokenizer
// gives each text below, which the Tokenizer Loam generates must give too.
//
//   npm install @huggingface/tokenizers@0.2.0
//   node tokenize.mjs <this directory> <a directory holding tokenizer_config.json>
import { readFileSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { Tokenizer } from '@huggingface/tokenizers';

const here = process.argv[2] ?? dirname(fileURLToPath(import.meta.url));
const configDir = process.argv[3] ?? here;
const tokenizer = new Tokenizer(
  JSON.parse(readFileSync(join(here, 'tokenizer.json'), 'utf8')),
  JSON.parse(readFileSync(join(configDir, 'tokenizer_config.json'), 'utf8')),
);

const TEXTS = [
  'Hello, my name is',
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

const tokenize = TEXTS.map((text) => ({ text, ids: tokenizer.encode(text, { add_special_tokens: false }).ids }));
writeFileSync(join(here, 'tokenize.json'), `${JSON.stringify({ tokenize }, null, 2)}\n`);
console.log(`${tokenize.length} texts`);
