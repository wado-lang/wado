// The ONNX protobuf the oracle scripts write, encoded by hand so that they need
// nothing past onnxruntime.

export const FLOAT = 1;
export const INT32 = 6;
export const INT64 = 7;

function varint(n) {
  const out = [];
  let v = BigInt.asUintN(64, BigInt(n));
  while (v >= 0x80n) {
    out.push(Number(v & 0x7fn) | 0x80);
    v >>= 7n;
  }
  out.push(Number(v));
  return out;
}

// A message as its fields, [number, value] each: a value is a varint (bigint
// or number), a Buffer (length-delimited), or an array of fields (a nested
// message).
export function message(fields) {
  const bytes = [];
  for (const [number, value] of fields) {
    if (Array.isArray(value) || Buffer.isBuffer(value)) {
      const body = Buffer.isBuffer(value) ? value : message(value);
      bytes.push(...varint((number << 3) | 2), ...varint(body.length), ...body);
    } else {
      bytes.push(...varint(number << 3), ...varint(value));
    }
  }
  return Buffer.from(bytes);
}

export const text = (s) => Buffer.from(s, 'utf8');

// TensorProto: dims = 1, data_type = 2, name = 8, raw_data = 9.
export function tensorProto(name, dataType, dims, raw) {
  return [...dims.map((d) => [1, d]), [2, dataType], [8, text(name)], [9, Buffer.from(raw)]];
}
