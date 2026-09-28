import assert from "node:assert/strict";
import { test } from "node:test";
import { convert } from "./hf2loam.mjs";

function safetensors(header, dataBytes) {
  const json = new TextEncoder().encode(JSON.stringify(header));
  const bytes = new Uint8Array(8 + json.length + dataBytes);
  new DataView(bytes.buffer).setBigUint64(0, BigInt(json.length), true);
  bytes.set(json, 8);
  return bytes;
}

const graph = safetensors({}, 0);

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
