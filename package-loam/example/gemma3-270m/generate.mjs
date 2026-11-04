// Writes generate.json, as ../../tools/greedy.mjs says, from build/model.onnx.
//
//   mise run loam-oracle-deps
//   node generate.mjs
import { dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { writeGenerated } from '../../tools/greedy.mjs';
import { decoder } from './decoder.mjs';

await writeGenerated({
  dir: dirname(fileURLToPath(import.meta.url)),
  prompts: [
    'Hello, my name is',
    '<start_of_turn>user\nWhat is the capital of France?<end_of_turn>\n<start_of_turn>model\n',
  ],
  decoder,
});
