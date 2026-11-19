// Writes tokenize.json: the ids Hugging Face's tokenizer gives each text below,
// which the Tokenizer Loam generates must give too.
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
  "It's what they've said: we'll see, I'd guess, you're right, I'm here.",
  "DON'T SHOUT'S, They'Re MiXeD",
  'parseHTMLString camelCase XMLHttpRequest',
  '  two leading spaces, and three   inside',
  'trailing spaces   ',
  'tab\there\nnewline\n\nblank line\r\n\r\nwindows',
  ' \n mixed \t\n whitespace',
  'digits 12345 and 3.14 and x2 and 1234567',
  'punctuation!!! ...?? (braces) [brackets] {curly} <angle> #hash @at',
  'fn main() {\n    let x = a/b;\n}\n// path/to/file\n',
  'café naïve Ωmega ǅungla',
  '日本語のテキスト、カタカナ。',
  'emoji 😀👍🏽 done',
  'combining é and ́alone',
  'rare 𝔘𝔫𝔦𝔠𝔬𝔡𝔢 and \u0001 control',
  '<|start|>user<|message|>What is 2+2?<|end|><|start|>assistant',
];

const tokenize = TEXTS.map((text) => ({ text, ids: tokenizer.encode(text, { add_special_tokens: false }).ids }));
writeFileSync(join(here, 'tokenize.json'), `${JSON.stringify({ tokenize }, null, 2)}\n`);
console.log(`${tokenize.length} texts`);
