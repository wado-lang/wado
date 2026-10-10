// What onnxruntime makes of a model: the graph transformers that changed it,
// where each node runs, the optimized graph with the shapes it infers, and one
// profiled run. Loam runs onnxruntime as an oracle and does not read its code,
// so this is how its decisions are seen.
//
//   mise run loam-ort-inspect <model.onnx> [--dim name=extent]... [--out dir]
//
// A symbolic dimension is bound by `--dim`, as Loam's `dims` option binds one;
// onnxruntime then optimizes for that extent. Every input is fed zeros.
import { spawnSync } from 'node:child_process';
import { mkdirSync, readdirSync, readFileSync, rmSync } from 'node:fs';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { parseArgs } from 'node:util';
import * as ort from 'onnxruntime-node';
import onnxProto from 'onnx-proto';

const { onnx } = onnxProto;

const { values, positionals } = parseArgs({
  allowPositionals: true,
  options: {
    dim: { type: 'string', multiple: true, default: [] },
    out: { type: 'string', default: 'target/ort-inspect' },
    session: { type: 'boolean', default: false },
  },
});
if (positionals.length !== 1) {
  console.error('usage: ort_inspect.mjs <model.onnx> [--dim name=extent]... [--out dir]');
  process.exit(2);
}
const [model] = positionals;
const dims = Object.fromEntries(
  values.dim.map((spec) => {
    const [name, extent] = spec.split('=');
    if (!/^\d+$/.test(extent ?? '')) {
      console.error(`--dim ${spec}: write name=extent`);
      process.exit(2);
    }
    return [name, Number(extent)];
  }),
);
const optimized = join(values.out, 'optimized.onnx');

if (values.session) {
  await runSession();
} else {
  report();
}

// Optimize and run the model once. onnxruntime writes its verbose log to the
// process's stderr below Node, which only a parent process can read, so
// `report` runs this as one.
async function runSession() {
  const session = await ort.InferenceSession.create(model, {
    graphOptimizationLevel: 'all',
    optimizedModelFilePath: optimized,
    freeDimensionOverrides: dims,
    logSeverityLevel: 0,
    logVerbosityLevel: 0,
    enableProfiling: true,
    profileFilePrefix: join(values.out, 'profile'),
  });
  // Every numeric type onnxruntime-node holds, by the array it reads.
  const elements = {
    float64: Float64Array,
    float32: Float32Array,
    float16: Uint16Array,
    int64: BigInt64Array,
    uint64: BigUint64Array,
    int32: Int32Array,
    uint32: Uint32Array,
    int16: Int16Array,
    uint16: Uint16Array,
    int8: Int8Array,
    uint8: Uint8Array,
    bool: Uint8Array,
  };
  const feeds = {};
  for (const input of session.inputMetadata) {
    const shape = input.shape.map((d) => {
      if (typeof d === 'number') return d;
      if (!(d in dims)) {
        console.error(`${input.name}: dimension '${d}' needs --dim ${d}=<extent>`);
        process.exit(2);
      }
      return dims[d];
    });
    const Elements = elements[input.type];
    if (!Elements) {
      console.error(`${input.name}: no zeros of type ${input.type}`);
      process.exit(2);
    }
    feeds[input.name] = new ort.Tensor(input.type, new Elements(shape.reduce((a, b) => a * b, 1)), shape);
  }
  await session.run(feeds);
  // The binding names the profile nowhere, so `report` finds it in `--out`.
  session.endProfiling();
}

// The profile onnxruntime writes under the `profileFilePrefix` it is given.
function isProfile(name) {
  return name.startsWith('profile_') && name.endsWith('.json');
}

function report() {
  // What an earlier run wrote is removed, and nothing else in `--out`, so the
  // one profile left after the run is this run's.
  mkdirSync(values.out, { recursive: true });
  for (const f of readdirSync(values.out)) {
    if (isProfile(f) || f === 'optimized.onnx') rmSync(join(values.out, f));
  }
  const child = spawnSync(
    process.execPath,
    [fileURLToPath(import.meta.url), '--session', ...process.argv.slice(2)],
    { encoding: 'utf8', maxBuffer: 1 << 30 },
  );
  if (child.status !== 0) {
    // The log is noise beside the failure; its last lines carry the reason.
    console.error(child.stderr.split('\n').slice(-20).join('\n'));
    process.exit(child.status ?? 1);
  }
  const [profileName] = readdirSync(values.out).filter(isProfile);
  const profile = join(values.out, profileName);
  const log = child.stderr.split('\n');

  console.log(`${model} under onnxruntime-node ${ort.env.versions.common}, every graph optimization`);

  console.log('\ntransformers that changed the graph');
  const changed = new Map();
  for (const line of log) {
    const m = line.match(/GraphTransformer (\S+) modified: (\d+)/);
    if (m && m[2] !== '0') changed.set(m[1], (changed.get(m[1]) ?? 0) + Number(m[2]));
  }
  for (const [name, times] of changed) console.log(`  ${name} x${times}`);

  console.log('\nplacements');
  for (const line of log) {
    const m = line.match(/VerifyEachNodeIsAssignedToAnEp\]\s+(.*)$/);
    if (m && m[1] !== 'Node placements') console.log(`  ${m[1].trim()}`);
  }

  console.log(`\noptimized graph (${optimized})`);
  const graph = onnx.ModelProto.decode(readFileSync(optimized)).graph;
  const shapes = new Map();
  for (const vi of [...graph.input, ...graph.valueInfo, ...graph.output]) {
    const dim = vi.type?.tensorType?.shape?.dim;
    if (dim) shapes.set(vi.name, `[${dim.map((d) => d.dimParam || String(d.dimValue)).join(', ')}]`);
  }
  for (const node of graph.node) {
    const op = node.domain ? `${node.opType}@${node.domain}` : node.opType;
    console.log(`  ${node.output.join(', ')} = ${op}(${node.input.join(', ')})`);
    for (const output of node.output) {
      if (shapes.has(output)) console.log(`      ${output}: ${shapes.get(output)}`);
    }
  }

  console.log(`\nprofiled run (${profile})`);
  const kernels = JSON.parse(readFileSync(profile, 'utf8')).filter(
    (e) => e.cat === 'Node' && e.name.endsWith('_kernel_time'),
  );
  for (const e of kernels) {
    const name = e.name.slice(0, -'_kernel_time'.length);
    console.log(`  ${name}  ${e.args.op_name}  ${e.args.provider}  ${e.dur}us`);
  }
}
