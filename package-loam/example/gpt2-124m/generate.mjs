// Writes generate.json beside model.onnx, as ../../tools/greedy.mjs says. The
// graph branches on whether it is given a past.
//
//   mise run loam-oracle-deps
//   node generate.mjs
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import * as ort from 'onnxruntime-node';
import { writeGenerated } from '../../tools/greedy.mjs';

const here = dirname(fileURLToPath(import.meta.url));
await writeGenerated({
  dir: here,
  model: join(here, 'model.onnx'),
  prompts: ['Hello, my name is'],
  layers: 12,
  emptyPast: [1, 12, 0, 64],
  feeds: (seen) => ({ use_cache_branch: new ort.Tensor('bool', [seen > 0], [1]) }),
});
