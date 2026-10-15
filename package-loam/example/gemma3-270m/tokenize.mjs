// Writes tokenize.json: the ids Hugging Face's tokenizer gives each text below,
// `<bos>` first, which the Tokenizer Loam generates must give too.
//
//   node tokenize.mjs [this directory]
import { readFileSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { Tokenizer } from '@huggingface/tokenizers';

const here = process.argv[2] ?? dirname(fileURLToPath(import.meta.url));
const build = join(here, 'build');
const tokenizer = new Tokenizer(
  JSON.parse(readFileSync(join(build, 'tokenizer.json'), 'utf8')),
  JSON.parse(readFileSync(join(build, 'tokenizer_config.json'), 'utf8')),
);

const TEXTS = [
  'Hello, my name is',
  "It's what they've said: we'll see, I'd guess.",
  '  two leading spaces, and three   inside',
  'trailing spaces   ',
  'tab\there\nnewline\n\nblank line',
  'digits 12345 and 3.14 and x2',
  'punctuation!!! ...?? (braces) [brackets] {curly} <angle> #hash @at',
  'café naïve Ωmega',
  '日本語のテキスト',
  'emoji 😀👍🏽 done',
  'rare 𝔘𝔫𝔦𝔠𝔬𝔡𝔢 and \u0001 control',
  '<start_of_turn>user\nWhat is the capital of France?<end_of_turn>\n<start_of_turn>model\n',
];

const tokenize = TEXTS.map((text) => ({ text, ids: tokenizer.encode(text).ids }));
writeFileSync(join(here, 'tokenize.json'), `${JSON.stringify({ tokenize }, null, 2)}\n`);
console.log(`${tokenize.length} texts`);
