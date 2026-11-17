// A decoder's ONNX graph with its greedy pick appended, so onnxruntime reads
// back the next token rather than the vocabulary's logits, as Loam's
// `arg_max` option does. Each form keeps the first of equal scores, as
// `ArgMax` does, and suits one provider:
//
// - `blocks`, for WebGPU: the best of each block of the last position's
//   logits, then the best of those. Its `ArgMax` over a whole vocabulary runs
//   2.3 times as long as reading the logits back.
// - `topk`, for CUDA: a `TopK` of one. Its `ArgMax` and `ReduceMax` need
//   cuDNN, which `fetch_onnxruntime_cuda.sh` does not fetch.
//
// The graph is edited as protobuf: the `GraphProto` gains the nodes and the
// output, and every other field of the model, its weights among them, is
// copied as it is.
//
// Run as a program, it writes the graph of an example's decoder and prints
// its path, for the benchmark's C++ arms:
//
//   node tools/picking.mjs example/gemma3-270m decoder topk
import assert from 'node:assert';
import { existsSync, readFileSync, statSync, writeFileSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

/** The output the appended nodes write: the next token, as an int64 `[1, 1]`. */
export const NEXT = 'loam.next';

/**
 * The path of `model` with the pick appended in `form`, written beside it
 * where it is missing or older than the model or this file, so external data
 * resolves alike.
 */
export function picking(model, vocab, form) {
  const nodes = { blocks: blockNodes, topk: topkNodes }[form];
  assert(nodes, `form must be blocks or topk, not ${form}`);
  const out = model.replace(/\.onnx$/, `.picking-${form}.onnx`);
  const written = existsSync(out) ? statSync(out).mtimeMs : 0;
  if (written < statSync(model).mtimeMs || written < statSync(fileURLToPath(import.meta.url)).mtimeMs) {
    const bytes = readFileSync(model);
    writeFileSync(out, appended(bytes, [...lastRow(), ...nodes(vocab, opset(bytes))]));
  }
  return out;
}

/** The model `bytes` with `nodes` and the output `NEXT` added to its graph. */
function appended(bytes, nodes) {
  const GRAPH = 7;
  const INT64 = 7;
  const output = Buffer.concat([text(1, NEXT), message(2, message(1, varintField(1, INT64)))]);
  return Buffer.concat(
    fields(bytes).map(({ number, value, raw }) =>
      number === GRAPH ? message(GRAPH, Buffer.concat([value, ...nodes.map((n) => message(1, n)), message(12, output)])) : raw,
    ),
  );
}

/** The version of the default operator set the model `bytes` imports. */
function opset(bytes) {
  const OPSET_IMPORT = 8;
  for (const f of fields(bytes)) {
    if (f.number !== OPSET_IMPORT) continue;
    const parts = fields(f.value);
    const domain = parts.find((p) => p.number === 1)?.value.toString() ?? '';
    if (domain === '' || domain === 'ai.onnx') return parts.find((p) => p.number === 2).value;
  }
  throw new Error('the model imports no default operator set');
}

/**
 * The top-level fields of the message `bytes`: a varint's value as a number,
 * a length-delimited one's as its bytes, and each field's bytes whole.
 */
function fields(bytes) {
  const out = [];
  let at = 0;
  while (at < bytes.length) {
    const start = at;
    let key;
    let value;
    [key, at] = readVarint(bytes, at);
    const wire = key & 7;
    if (wire === 0) {
      [value, at] = readVarint(bytes, at);
    } else if (wire === 2) {
      let length;
      [length, at] = readVarint(bytes, at);
      value = bytes.subarray(at, at + length);
      at += length;
    } else {
      throw new Error(`a field of wire type ${wire}`);
    }
    out.push({ number: key >> 3, value, raw: bytes.subarray(start, at) });
  }
  return out;
}

const n = (name) => `loam.pick.${name}`;
const ints = (name, values) => node('Constant', [], n(name), [intsAttr('value_ints', values)]);

/** The nodes writing `last`, the last position of `logits`, `[1, 1, vocab]`. */
function lastRow() {
  return [
    ints('axis', [1]),
    ints('one', [1]),
    ints('end', [2 ** 53 - 1]),
    ints('pair', [1, 1]),
    node('Shape', ['logits'], n('dims')),
    node('Gather', [n('dims'), n('axis')], n('rows')),
    node('Sub', [n('rows'), n('one')], n('from')),
    node('Slice', ['logits', n('from'), n('end'), n('axis')], n('last')),
  ];
}

/** `last`'s greatest score's index, by a `TopK` of one. */
function topkNodes() {
  return [
    node('TopK', [n('last'), n('one')], [n('score'), n('token')], [intAttr('axis', -1)]),
    node('Reshape', [n('token'), n('pair')], NEXT),
  ];
}

/**
 * `last`'s greatest score's index, by blocks, in the default operator set's
 * `version`: from 18, `ReduceMax` reads its axes as an input rather than an
 * attribute.
 */
function blockNodes(vocab, version) {
  const blocks = largestDivisorUpTo(vocab, Math.floor(Math.sqrt(vocab)));
  return [
    ints('grid', [blocks, vocab / blocks]),
    node('Constant', [], n('width'), [intAttr('value_int', vocab / blocks)]),
    node('Reshape', [n('last'), n('grid')], n('blocks')),
    node('ArgMax', [n('blocks')], n('inner'), [intAttr('axis', 1), intAttr('keepdims', 0)]),
    version >= 18
      ? node('ReduceMax', [n('blocks'), n('axis')], n('peaks'), [intAttr('keepdims', 0)])
      : node('ReduceMax', [n('blocks')], n('peaks'), [intsAttr('axes', [1]), intAttr('keepdims', 0)]),
    node('ArgMax', [n('peaks')], n('block'), [intAttr('axis', 0), intAttr('keepdims', 0)]),
    node('Gather', [n('inner'), n('block')], n('within'), [intAttr('axis', 0)]),
    node('Mul', [n('block'), n('width')], n('start')),
    node('Add', [n('start'), n('within')], n('token')),
    node('Reshape', [n('token'), n('pair')], NEXT),
  ];
}

function largestDivisorUpTo(value, bound) {
  for (let d = bound; d > 1; d--) if (value % d === 0) return d;
  return 1;
}

/** A node writing `outputs`, one name or several, named after the first. */
function node(type, inputs, outputs, attributes = []) {
  const written = [outputs].flat();
  return Buffer.concat([
    ...inputs.map((i) => text(1, i)),
    ...written.map((o) => text(2, o)),
    text(3, written[0]),
    text(4, type),
    ...attributes,
  ]);
}

function intAttr(name, value) {
  const INT = 2;
  return message(5, Buffer.concat([text(1, name), varintField(3, value), varintField(20, INT)]));
}

function intsAttr(name, values) {
  const INTS = 7;
  return message(5, Buffer.concat([text(1, name), ...values.map((v) => varintField(8, v)), varintField(20, INTS)]));
}

/** `value` as a protobuf varint, a negative one in 64-bit two's complement. */
function varint(value) {
  let v = BigInt.asUintN(64, BigInt(value));
  const out = [];
  while (v > 127n) {
    out.push(Number(v & 127n) | 128);
    v >>= 7n;
  }
  out.push(Number(v));
  return out;
}

function readVarint(bytes, at) {
  let value = 0;
  let scale = 1;
  for (;;) {
    const b = bytes[at++];
    value += (b & 127) * scale;
    scale *= 128;
    if (b < 128) return [value, at];
  }
}

const varintField = (number, value) => Buffer.from([...varint(number * 8), ...varint(value)]);
const message = (number, bytes) => Buffer.concat([Buffer.from([...varint(number * 8 + 2), ...varint(bytes.length)]), bytes]);
const text = (number, s) => message(number, Buffer.from(s));

if (import.meta.url === pathToFileURL(process.argv[1]).href) {
  const [dir, name, form] = process.argv.slice(2);
  const { model, vocab } = (await import(pathToFileURL(join(resolve(dir), 'decoder.mjs')).href))[name];
  console.log(picking(model, vocab, form));
}
