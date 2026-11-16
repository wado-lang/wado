// Writes generate.json from build/model.onnx, generate_fp16.json and
// build/logits_fp16.bin from build/model_fp16.onnx, and generate_q4.json and
// build/logits_q4.bin from build/model_q4.onnx, as ../../tools/greedy.mjs
// says. Naming some of the files writes only those.
//
//   mise run loam-oracle-deps
//   node generate.mjs [generate_q4.json]...
import { dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { writeGenerated } from '../../tools/greedy.mjs';
import { decoder, decoderFp16, decoderQ4 } from './decoder.mjs';

const written = {
  dir: dirname(fileURLToPath(import.meta.url)),
  prompts: [
    'Hello, my name is',
    '<start_of_turn>user\nWhat is the capital of France?<end_of_turn>\n<start_of_turn>model\n',
  ],
};
const outputs = [
  { decoder, name: 'generate.json' },
  { decoder: decoderFp16, name: 'generate_fp16.json', logits: 'build/logits_fp16.bin' },
  { decoder: decoderQ4, name: 'generate_q4.json', logits: 'build/logits_q4.bin' },
];
const named = process.argv.slice(2);
for (const output of outputs) {
  if (named.length === 0 || named.includes(output.name)) await writeGenerated({ ...written, ...output });
}
