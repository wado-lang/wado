// decoder_model_merged.onnx as ../../tools/greedy.mjs decodes it. The graph
// branches on whether it is given a past.
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import * as ort from 'onnxruntime-node';

export const decoder = {
  model: join(dirname(fileURLToPath(import.meta.url)), 'decoder_model_merged.onnx'),
  layers: 12,
  emptyPast: [1, 12, 0, 64],
  vocab: 50257,
  feeds: (seen) => ({ use_cache_branch: new ort.Tensor('bool', [seen > 0], [1]) }),
};
