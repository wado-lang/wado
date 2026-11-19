// A model with every `accuracy_level` attribute of its graph's nodes removed.
// At level 4, onnxruntime's CPU quantizes a `MatMulNBits` input to int8, which
// moves gpt-oss-20b's logits by up to 1.9 and flips which experts a token
// routes to where two score within a few f16 ulps. Loam computes the product
// in f32, so an oracle holding it to the tokens onnxruntime picks runs the
// operator at its default, as exact as the input's float.
import { existsSync, readFileSync, statSync, writeFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { fields, message } from './protobuf.mjs';

const GRAPH = 7;
const NODE = 1;
const ATTRIBUTE = 5;
const NAME = 1;

/**
 * The path of `model` without `accuracy_level`, written beside it where it is
 * missing or older than the model or this file, so external data resolves
 * alike; `model` itself where no node sets the attribute.
 */
export function exact(model) {
  const out = model.replace(/\.onnx$/, '.exact.onnx');
  const written = existsSync(out) ? statSync(out).mtimeMs : 0;
  if (written >= statSync(model).mtimeMs && written >= statSync(fileURLToPath(import.meta.url)).mtimeMs) return out;
  const bytes = readFileSync(model);
  let removed = 0;
  const node = (bytes) =>
    Buffer.concat(
      fields(bytes).flatMap(({ number, value, raw }) => {
        const approximate = number === ATTRIBUTE && fields(value).some((a) => a.number === NAME && `${a.value}` === 'accuracy_level');
        if (approximate) removed++;
        return approximate ? [] : [raw];
      }),
    );
  const graph = (bytes) => Buffer.concat(fields(bytes).map(({ number, value, raw }) => (number === NODE ? message(NODE, node(value)) : raw)));
  const edited = Buffer.concat(fields(bytes).map(({ number, value, raw }) => (number === GRAPH ? message(GRAPH, graph(value)) : raw)));
  if (removed === 0) return model;
  writeFileSync(out, edited);
  return out;
}
