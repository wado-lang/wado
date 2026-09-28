// Rebuild the checkpoint gpt2.wado loads from Hugging Face's model.safetensors:
// the ONNX graph names each tensor `transformer.<hf name>`, and its LM head
// (`onnx::MatMul_3718`) is the token embedding transposed. An alternative to
// fetch.sh that runs no Wado:
//
//   curl -fL -o hf.safetensors https://huggingface.co/openai-community/gpt2/resolve/607a30d783dfa663caf39e06633721c8d4cfcd7e/model.safetensors
//   node --max-old-space-size=4096 hf2loam.mjs hf.safetensors gpt2-header.safetensors gpt2.safetensors
//
// `convert` touches no Node API, so a browser page can import it too.

const ITEM_BYTES = { F32: 4 };
const LM_HEAD = "onnx::MatMul_3718";

// The same two checks as `header_end` in src/runtime/checkpoint.wado.
function readHeader(bytes) {
  if (bytes.length < 8) throw new Error(`the header is 8 bytes, past the ${bytes.length} given`);
  const n = Number(new DataView(bytes.buffer, bytes.byteOffset, 8).getBigUint64(0, true));
  if (n > bytes.length - 8) throw new Error(`the header is ${n} bytes, past the ${bytes.length - 8} given`);
  return { n, json: JSON.parse(new TextDecoder().decode(bytes.subarray(8, 8 + n))) };
}

function expect(name, what, got, want) {
  if (JSON.stringify(got) !== JSON.stringify(want)) {
    throw new Error(`${name}: ${what} ${JSON.stringify(got)}, the graph wants ${JSON.stringify(want)}`);
  }
}

/**
 * Converts Hugging Face's `model.safetensors` (`hf`) into the checkpoint the
 * graph's header (`header`, gpt2-header.safetensors) describes. Both arguments
 * and the result are `Uint8Array`s.
 */
export function convert(hf, header) {
  const { n: hfN, json: hfH } = readHeader(hf);
  const want = readHeader(header).json;
  delete want.__metadata__;
  const dataBytes = hf.length - 8 - hfN;

  function hfTensor(name) {
    const t = hfH[name];
    if (!t) throw new Error(`missing ${name}`);
    const item = ITEM_BYTES[t.dtype];
    if (!item) throw new Error(`${name}: dtype ${t.dtype} is not supported`);
    const [b, e] = t.data_offsets;
    if (!Number.isSafeInteger(b) || !Number.isSafeInteger(e) || b < 0 || b > e || e > dataBytes) {
      throw new Error(`${name}: data_offsets [${b}, ${e}] lie outside the file's ${dataBytes} data bytes`);
    }
    const bytes = hf.subarray(8 + hfN + b, 8 + hfN + e);
    const size = item * t.shape.reduce((n, d) => n * d, 1);
    if (bytes.length !== size) {
      throw new Error(`${name}: ${bytes.length} bytes, its shape ${JSON.stringify(t.shape)} needs ${size}`);
    }
    return { dtype: t.dtype, shape: t.shape, bytes };
  }

  const sources = [];
  const outHeader = {};
  let off = 0;
  for (const [name, t] of Object.entries(want)) {
    const transpose = name === LM_HEAD;
    const src = hfTensor(transpose ? "wte.weight" : name.replace(/^transformer\./, ""));
    expect(name, "dtype", src.dtype, t.dtype);
    expect(name, transpose ? "transposed shape" : "shape", transpose ? [...src.shape].reverse() : src.shape, t.shape);
    outHeader[name] = { dtype: t.dtype, shape: t.shape, data_offsets: [off, off + src.bytes.length] };
    sources.push({ src, transpose, at: off });
    off += src.bytes.length;
  }

  // Padded with spaces to a multiple of 8, as `write_safetensors` pads it, so each
  // tensor starts 8-byte aligned and the header is gpt2-header.safetensors itself.
  let json = JSON.stringify(outHeader);
  json += " ".repeat((8 - (json.length % 8)) % 8);
  const h = new TextEncoder().encode(json);
  const base = 8 + h.length;
  const out = new Uint8Array(base + off);
  new DataView(out.buffer).setBigUint64(0, BigInt(h.length), true);
  out.set(h, 8);
  for (const { src, transpose, at } of sources) {
    if (!transpose) {
      out.set(src.bytes, base + at);
      continue;
    }
    const [rows, cols] = src.shape;
    // A Float32Array view needs a 4-byte-aligned offset, which the source's
    // header length does not promise, so it reads a copy. (Node's
    // `Buffer#slice` is a view, hence the constructor.)
    const from = new Float32Array(new Uint8Array(src.bytes).buffer);
    const to = new Float32Array(out.buffer, base + at, rows * cols);
    for (let r = 0; r < rows; r++) for (let c = 0; c < cols; c++) to[c * rows + r] = from[r * cols + c];
  }
  return out;
}

if (globalThis.process?.argv?.[1]?.endsWith("hf2loam.mjs")) {
  const { readFileSync, writeFileSync } = await import("node:fs");
  const [hfPath, headerPath, outPath] = process.argv.slice(2);
  const out = convert(readFileSync(hfPath), readFileSync(headerPath));
  writeFileSync(outPath, out);
  console.log(`wrote ${outPath}: ${out.length} bytes`);
}
