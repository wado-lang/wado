// The protobuf wire format, as far as the tools that edit an ONNX model in
// place need it: the fields they change are rebuilt, and every other field,
// the weights among them, is copied as it is.

/**
 * The top-level fields of the message `bytes`: a varint's value as a number,
 * a length-delimited one's as its bytes, a fixed one's as its bytes, and each
 * field's bytes whole.
 */
export function fields(bytes) {
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
    } else if (wire === 1 || wire === 5) {
      const length = wire === 1 ? 8 : 4;
      value = bytes.subarray(at, at + length);
      at += length;
    } else {
      throw new Error(`a field of wire type ${wire}`);
    }
    out.push({ number: key >> 3, value, raw: bytes.subarray(start, at) });
  }
  return out;
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

export const varintField = (number, value) => Buffer.from([...varint(number * 8), ...varint(value)]);
export const message = (number, bytes) => Buffer.concat([Buffer.from([...varint(number * 8 + 2), ...varint(bytes.length)]), bytes]);
export const text = (number, s) => message(number, Buffer.from(s));
