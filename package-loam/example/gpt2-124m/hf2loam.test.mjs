import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { convert, layoutOf } from "./hf2loam.mjs";

function safetensors(header, dataBytes) {
  const json = new TextEncoder().encode(JSON.stringify(header));
  const bytes = new Uint8Array(8 + json.length + dataBytes);
  new DataView(bytes.buffer).setBigUint64(0, BigInt(json.length), true);
  bytes.set(json, 8);
  return bytes;
}

const graph = "g () => ()\n<>\n{\n}";

test("the layout is the graph's initializers declared without values, in order", () => {
  const text = 'g (float[2] x) => (float[2] y)\n<float[2, 3] "a.w", int64[1] k = {7}, float[3] b>\n{\n}';
  assert.deepEqual(layoutOf(text), {
    "a.w": { dtype: "F32", shape: [2, 3] },
    b: { dtype: "F32", shape: [3] },
  });
});

test("GPT-2's graph lays out its 149 weights, the LM head last", () => {
  const layout = layoutOf(readFileSync(new URL("./gpt2.onnxtext", import.meta.url), "utf8"));
  const names = Object.keys(layout);
  assert.equal(names.length, 149);
  assert.deepEqual(layout[names[0]], { dtype: "F32", shape: [50257, 768] });
  assert.equal(names.at(-1), "onnx::MatMul_3718");
});

test("a checkpoint shorter than the length prefix is rejected by name", () => {
  assert.throws(() => convert(new Uint8Array(5), graph), { message: "the header is 8 bytes, past the 5 given" });
});

test("a header declared past the end of the checkpoint is rejected by name", () => {
  const whole = safetensors({ "h.0.ln_1.weight": { dtype: "F32", shape: [1], data_offsets: [0, 4] } }, 4);
  const truncated = whole.subarray(0, 20);
  const declared = whole.length - 4 - 8;
  assert.ok(declared > truncated.length - 8);
  assert.throws(() => convert(truncated, graph), { message: `the header is ${declared} bytes, past the 12 given` });
});
