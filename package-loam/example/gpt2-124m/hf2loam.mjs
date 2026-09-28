// Rebuild the checkpoint gpt2.wado loads from Hugging Face's model.safetensors:
// the ONNX graph names each tensor `transformer.<hf name>`, and its LM head
// (`onnx::MatMul_3718`) is the token embedding transposed. An alternative to
// fetch.sh that runs no Wado:
//
//   curl -fL -o hf.safetensors https://huggingface.co/openai-community/gpt2/resolve/607a30d783dfa663caf39e06633721c8d4cfcd7e/model.safetensors
//   node --max-old-space-size=4096 hf2loam.mjs hf.safetensors gpt2-header.safetensors gpt2.safetensors
import { readFileSync, writeFileSync } from "node:fs";

const [hfPath, headerPath, outPath] = process.argv.slice(2);

function readHeader(buf) {
  const n = Number(buf.readBigUInt64LE(0));
  return { n, json: JSON.parse(buf.subarray(8, 8 + n).toString("utf8")) };
}

const hf = readFileSync(hfPath);
const { n: hfN, json: hfH } = readHeader(hf);
const want = readHeader(readFileSync(headerPath)).json;
delete want.__metadata__;

function hfTensor(name) {
  const t = hfH[name];
  if (!t) throw new Error(`missing ${name}`);
  const [b, e] = t.data_offsets;
  return { shape: t.shape, bytes: hf.subarray(8 + hfN + b, 8 + hfN + e) };
}

const parts = [];
const header = {};
let off = 0;
for (const [name, t] of Object.entries(want)) {
  let bytes;
  if (name === "onnx::MatMul_3718") {
    const src = hfTensor("wte.weight");
    const [rows, cols] = src.shape;
    const f = new Float32Array(src.bytes.buffer.slice(src.bytes.byteOffset, src.bytes.byteOffset + src.bytes.length));
    const out = new Float32Array(rows * cols);
    for (let r = 0; r < rows; r++) for (let c = 0; c < cols; c++) out[c * rows + r] = f[r * cols + c];
    bytes = Buffer.from(out.buffer);
  } else {
    const src = hfTensor(name.replace(/^transformer\./, ""));
    if (JSON.stringify(src.shape) !== JSON.stringify(t.shape)) throw new Error(`shape ${name}`);
    bytes = src.bytes;
  }
  header[name] = { dtype: t.dtype, shape: t.shape, data_offsets: [off, off + bytes.length] };
  off += bytes.length;
  parts.push(bytes);
}
const h = Buffer.from(JSON.stringify(header));
const len = Buffer.alloc(8);
len.writeBigUInt64LE(BigInt(h.length));
writeFileSync(outPath, Buffer.concat([len, h, ...parts]));
console.log(`wrote ${outPath}: ${Object.keys(header).length} tensors, ${off} bytes`);
