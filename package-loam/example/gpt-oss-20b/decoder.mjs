// build/model_q4f16.onnx as ../../tools/greedy.mjs decodes it.
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

export const decoder = {
  model: join(dirname(fileURLToPath(import.meta.url)), 'build', 'model_q4f16.onnx'),
  layers: 24,
  emptyPast: [1, 8, 0, 64],
  vocab: 201088,
  half: true,
};
