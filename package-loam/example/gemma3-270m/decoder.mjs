// build/model.onnx, build/model_fp16.onnx and build/model_q4.onnx as
// ../../tools/greedy.mjs decodes them.
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const build = join(dirname(fileURLToPath(import.meta.url)), 'build');

export const decoder = {
  model: join(build, 'model.onnx'),
  layers: 18,
  emptyPast: [1, 1, 0, 256],
};

export const decoderFp16 = { ...decoder, model: join(build, 'model_fp16.onnx'), half: true };

export const decoderQ4 = { ...decoder, model: join(build, 'model_q4.onnx') };
