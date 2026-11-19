// Writes generate.json and build/logits.bin from build/model_q4f16.onnx, as
// ../../tools/greedy.mjs says. Its weights outgrow an 8 GB device, so
// onnxruntime runs it on the CPU alone.
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
    '<|start|>user<|message|>What is 2+2?<|end|><|start|>assistant',
  ],
  // `<|return|>` and `<|endoftext|>`, generation_config.json's `eos_token_id`.
  ends: [200002, 199999],
  decoder,
  logits: 'build/logits.bin',
  webgpu: false,
});
