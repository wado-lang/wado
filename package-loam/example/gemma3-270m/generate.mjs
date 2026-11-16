// Writes generate.json from build/model.onnx, and for each export X of fp16,
// q4 and q4f16, generate_X.json and build/logits_X.bin from
// build/model_X.onnx, as ../../tools/greedy.mjs says. Naming some of the
// files writes only those.
//
//   mise run loam-oracle-deps
//   node generate.mjs [generate_q4.json]...
import { dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { writeGenerated } from '../../tools/greedy.mjs';
import { decoder, decoderFp16, decoderQ4, decoderQ4f16 } from './decoder.mjs';

const written = {
  dir: dirname(fileURLToPath(import.meta.url)),
  prompts: [
    'Hello, my name is',
    '<start_of_turn>user\nWhat is the capital of France?<end_of_turn>\n<start_of_turn>model\n',
  ],
  // `<eos>` and `<end_of_turn>`, generation_config.json's `eos_token_id`.
  ends: [1, 106],
};
const outputs = [
  { decoder, name: 'generate.json' },
  { decoder: decoderFp16, name: 'generate_fp16.json', logits: 'build/logits_fp16.bin' },
  { decoder: decoderQ4, name: 'generate_q4.json', logits: 'build/logits_q4.bin' },
  { decoder: decoderQ4f16, name: 'generate_q4f16.json', logits: 'build/logits_q4f16.bin' },
];
const named = process.argv.slice(2);
for (const output of outputs) {
  if (named.length === 0 || named.includes(output.name)) await writeGenerated({ ...written, ...output });
}
