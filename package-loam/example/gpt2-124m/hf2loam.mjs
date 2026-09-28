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

const ITEM_BYTES = { F32: 4 };

function hfTensor(name) {
  const t = hfH[name];
  if (!t) throw new Error(`missing ${name}`);
  const item = ITEM_BYTES[t.dtype];
  if (!item) throw new Error(`${name}: dtype ${t.dtype} is not supported`);
  const [b, e] = t.data_offsets;
  const dataBytes = hf.length - 8 - hfN;
  if (!Number.isSafeInteger(b) || !Number.isSafeInteger(e) || b < 0 || b > e || e > dataBytes) {
    throw new Error(`${name}: data_offsets [${b}, ${e}] lie outside the file's ${dataBytes} data bytes`);
  }
  const bytes = hf.subarray(8 + hfN + b, 8 + hfN + e);
  const want = item * t.shape.reduce((n, d) => n * d, 1);
  if (bytes.length !== want) {
    throw new Error(`${name}: ${bytes.length} bytes, its shape ${JSON.stringify(t.shape)} needs ${want}`);
  }
  return { dtype: t.dtype, shape: t.shape, bytes };
}

function expect(name, what, got, want) {
  if (JSON.stringify(got) !== JSON.stringify(want)) {
    throw new Error(`${name}: ${what} ${JSON.stringify(got)}, the graph wants ${JSON.stringify(want)}`);
  }
}

const parts = [];
const header = {};
let off = 0;
for (const [name, t] of Object.entries(want)) {
  let bytes;
  if (name === "onnx::MatMul_3718") {
    const src = hfTensor("wte.weight");
    expect(name, "dtype", src.dtype, t.dtype);
    expect(name, "transposed shape", [...src.shape].reverse(), t.shape);
    const [rows, cols] = src.shape;
    const f = new Float32Array(src.bytes.buffer.slice(src.bytes.byteOffset, src.bytes.byteOffset + src.bytes.length));
    const out = new Float32Array(rows * cols);
    for (let r = 0; r < rows; r++) for (let c = 0; c < cols; c++) out[c * rows + r] = f[r * cols + c];
    bytes = Buffer.from(out.buffer);
  } else {
    const src = hfTensor(name.replace(/^transformer\./, ""));
    expect(name, "dtype", src.dtype, t.dtype);
    expect(name, "shape", src.shape, t.shape);
    bytes = src.bytes;
  }
  header[name] = { dtype: t.dtype, shape: t.shape, data_offsets: [off, off + bytes.length] };
  off += bytes.length;
  parts.push(bytes);
}
// Padded with spaces to a multiple of 8, as `write_safetensors` pads it, so each
// tensor starts 8-byte aligned and the header is gpt2-header.safetensors itself.
let json = JSON.stringify(header);
json += " ".repeat((8 - (json.length % 8)) % 8);
const h = Buffer.from(json);
const len = Buffer.alloc(8);
len.writeBigUInt64LE(BigInt(h.length));
writeFileSync(outPath, Buffer.concat([len, h, ...parts]));
console.log(`wrote ${outPath}: ${Object.keys(header).length} tensors, ${off} bytes`);
