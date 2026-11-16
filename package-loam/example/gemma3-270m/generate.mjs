// Writes generate.json from build/model.onnx, and generate_fp16.json and
// build/logits_fp16.bin from build/model_fp16.onnx, as ../../tools/greedy.mjs
// says.
//
//   mise run loam-oracle-deps
//   node generate.mjs
import { dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { writeGenerated } from '../../tools/greedy.mjs';
import { decoder, decoderFp16 } from './decoder.mjs';

const written = {
  dir: dirname(fileURLToPath(import.meta.url)),
  prompts: [
    'Hello, my name is',
    '<start_of_turn>user\nWhat is the capital of France?<end_of_turn>\n<start_of_turn>model\n',
  ],
};
await writeGenerated({ ...written, decoder });
await writeGenerated({ ...written, decoder: decoderFp16, name: 'generate_fp16.json', logits: 'build/logits_fp16.bin' });
