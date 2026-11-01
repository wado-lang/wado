// Writes generate.json, as ../../tools/greedy.mjs says, from build/model.onnx.
//
//   mise run loam-oracle-deps
//   node generate.mjs
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { writeGenerated } from '../../tools/greedy.mjs';

const here = dirname(fileURLToPath(import.meta.url));
await writeGenerated({
  dir: here,
  model: join(here, 'build', 'model.onnx'),
  prompts: [
    'Hello, my name is',
    '<start_of_turn>user\nWhat is the capital of France?<end_of_turn>\n<start_of_turn>model\n',
  ],
  layers: 18,
  emptyPast: [1, 1, 0, 256],
});
