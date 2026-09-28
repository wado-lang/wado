// The ONNX protobuf the oracle scripts write, encoded by hand so that they need
// nothing past onnxruntime.

export const FLOAT = 1;
export const INT32 = 6;
export const INT64 = 7;
export const BOOL = 9;
export const FLOAT16 = 10;
export const BFLOAT16 = 16;

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

// A float field travels as fixed32, which `message` writes for a value `f32` wraps.
export const f32 = (x) => ({ f32: x });

// A message as its fields, [number, value] each: a value is a varint (bigint
// or number), a float `f32` wraps, a Buffer (length-delimited), or an array of
// fields (a nested message).
export function message(fields) {
  const bytes = [];
  for (const [number, value] of fields) {
    if (Array.isArray(value) || Buffer.isBuffer(value)) {
      const body = Buffer.isBuffer(value) ? value : message(value);
      bytes.push(...varint((number << 3) | 2), ...varint(body.length), ...body);
    } else if (value.f32 !== undefined) {
      const body = Buffer.alloc(4);
      body.writeFloatLE(value.f32);
      bytes.push(...varint((number << 3) | 5), ...body);
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

// ValueInfoProto: name = 1, type = 2; TypeProto.tensor_type = 1;
// Tensor: elem_type = 1, shape = 2; TensorShapeProto.dim = 1;
// Dimension: dim_value = 1, dim_param = 2. `dims` null states no shape.
export function valueInfo(name, elemType, dims) {
  const dim = (d) => [1, typeof d === 'string' ? [[2, text(d)]] : [[1, d]]];
  const shape = dims === null ? [] : [[2, dims.map(dim)]];
  return [[1, text(name)], [2, [[1, [[1, elemType], ...shape]]]]];
}

// AttributeProto: name = 1, f = 2, i = 3, s = 4, t = 5, floats = 7, ints = 8,
// type = 20, whose FLOAT = 1, INT = 2, STRING = 3, TENSOR = 4, FLOATS = 6 and
// INTS = 7. A value is a number (INT), a string, one `f32` wraps, a list of
// either kind of number, or `{ tensor }` holding a TensorProto's fields.
function attribute(name, value) {
  const named = [1, text(name)];
  if (typeof value === 'string') return [named, [4, text(value)], [20, 3]];
  if (value.tensor) return [named, [5, value.tensor], [20, 4]];
  if (value.f32 !== undefined) return [named, [2, value], [20, 1]];
  if (Array.isArray(value)) {
    return value.length > 0 && value[0].f32 !== undefined
      ? [named, ...value.map((v) => [7, v]), [20, 6]]
      : [named, ...value.map((v) => [8, v]), [20, 7]];
  }
  return [named, [3, value], [20, 2]];
}

// A node as `nodeProto` takes it, `outputs` one name or a list of them.
export const node = (op, inputs, outputs, attrs = {}) => ({ op, inputs, outputs: [outputs].flat(), attrs });

// NodeProto: input = 1, output = 2, op_type = 4, attribute = 5. `attrs` maps
// each attribute's name to its value, in the order they are written.
export function nodeProto({ op, inputs, outputs, attrs }) {
  return [
    ...inputs.map((i) => [1, text(i)]),
    ...outputs.map((o) => [2, text(o)]),
    [4, text(op)],
    ...Object.entries(attrs).map(([name, value]) => [5, attribute(name, value)]),
  ];
}

// ModelProto: ir_version = 1, graph = 7, opset_import = 8 (version = 2).
export function modelProto(graph, opset) {
  return message([[1, 8], [7, graph], [8, [[2, opset]]]]);
}
