// build/model.onnx and its fp16, q4 and q4f16 exports as
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

export const decoderQ4f16 = { ...decoderFp16, model: join(build, 'model_q4f16.onnx') };
