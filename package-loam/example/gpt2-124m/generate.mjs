// Writes generate.json beside model.onnx, as ../../tools/greedy.mjs says.
//
//   mise run loam-oracle-deps
//   node generate.mjs
import { dirname } from 'node:path';
import { fileURLToPath } from 'node:url';
import { writeGenerated } from '../../tools/greedy.mjs';
import { decoder } from './decoder.mjs';

await writeGenerated({
  dir: dirname(fileURLToPath(import.meta.url)),
  prompts: ['Hello, my name is'],
  decoder,
});
