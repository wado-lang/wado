// build/model.onnx as ../../tools/greedy.mjs decodes it.
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

export const decoder = {
  model: join(dirname(fileURLToPath(import.meta.url)), 'build', 'model.onnx'),
  layers: 18,
  emptyPast: [1, 1, 0, 256],
};
