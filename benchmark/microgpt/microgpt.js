// microgpt training and inference benchmark for JavaScript.
//
// A port of Andrej Karpathy's dependency-free Python microgpt.
// Copyright (c) Andrej Karpathy, MIT License.
// https://gist.github.com/karpathy/8627fe009c40f57531cb18360106ce95
//
// The same program as microgpt.wado: a scalar autograd graph where a node holds
// its children directly, so both arms measure a GC'd language walking an object
// graph. Both phases reseed, so the two print the same loss and the same sample
// and can be checked against each other.
//
// How to run:
//   node benchmark/microgpt/microgpt.js

const ITERATIONS = 3;
const WARMUP = 1;

function printThroughput(label, workPerIter, n, elapsedNs, unit) {
  const secs = elapsedNs / 1e9;
  const rate = secs > 0 ? (workPerIter * n) / secs : 0;
  const perMs = elapsedNs / n / 1e6;
  let rbuf;
  if (rate >= 1e9) rbuf = `${(rate / 1e9).toFixed(2)} G ${unit}/s`;
  else if (rate >= 1e6) rbuf = `${(rate / 1e6).toFixed(2)} M ${unit}/s`;
  else if (rate >= 1e3) rbuf = `${(rate / 1e3).toFixed(2)} k ${unit}/s`;
  else rbuf = `${rate.toFixed(2)} ${unit}/s`;
  console.log(`${label}: ${rbuf}   (${perMs.toFixed(3)} ms/iter, ${n} iter)`);
}

// One step per document, so a measured iteration is one pass over the corpus.
const STEPS = 32;
// Each sample runs the full attention window, deeper than any training step
// reaches on this corpus.
const SAMPLES = 24;
const TEMPERATURE = 0.5;
const SEED = 42n;

// Let there be a deterministic source of chaos: `core:prng`'s, so the Wado arm
// matches — `Seed::from_u64` expands through SplitMix64, `Xoshiro256pp` draws,
// Box-Muller shapes. Python seeds Mersenne Twister, so the two draw differently.
const MASK64 = (1n << 64n) - 1n;

function rotl(x, n) {
  return ((x << n) | (x >> (64n - n))) & MASK64;
}

class Rng {
  constructor(seed) {
    let state = seed & MASK64;
    const splitmix = () => {
      state = (state + 0x9e3779b97f4a7c15n) & MASK64;
      let z = state;
      z = ((z ^ (z >> 30n)) * 0xbf58476d1ce4e5b9n) & MASK64;
      z = ((z ^ (z >> 27n)) * 0x94d049bb133111ebn) & MASK64;
      return (z ^ (z >> 31n)) & MASK64;
    };
    this.s = [splitmix(), splitmix(), splitmix(), splitmix()];
  }

  nextU64() {
    const s = this.s;
    const result = (rotl((s[0] + s[3]) & MASK64, 23n) + s[0]) & MASK64;
    const t = (s[1] << 17n) & MASK64;
    s[2] ^= s[0];
    s[3] ^= s[1];
    s[1] ^= s[2];
    s[0] ^= s[3];
    s[2] ^= t;
    s[3] = rotl(s[3], 45n);
    return result;
  }

  // 53 bits, the mantissa of a double, scaled into [0, 1).
  random() {
    return Number(this.nextU64() >> 11n) / 9007199254740992;
  }

  gauss(std) {
    let u = this.random();
    while (u <= 0) u = this.random();
    const v = this.random();
    return std * Math.sqrt(-2 * Math.log(u)) * Math.cos(2 * Math.PI * v);
  }

  // `random.choices(range(n), weights)`: one draw against the cumulative sum.
  choice(weights) {
    let total = 0;
    for (const w of weights) total += w;
    let r = this.random() * total;
    for (let i = 0; i < weights.length; i++) {
      r -= weights[i];
      if (r < 0) return i;
    }
    return weights.length - 1;
  }
}

// Let there be Autograd to recursively apply the chain rule through a
// computation graph. `Graph` is the heap those nodes live in; `id` is a node's
// slot in it, which makes the `visited` set a dense array.
class Value {
  constructor(id, data, children, localGrads) {
    this.id = id;
    this.data = data;
    this.grad = 0;
    this.children = children;
    this.localGrads = localGrads;
  }
}

class Graph {
  constructor() {
    this.nodes = [];
  }

  len() {
    return this.nodes.length;
  }

  value(data, children, localGrads) {
    const v = new Value(this.nodes.length, data, children, localGrads);
    this.nodes.push(v);
    return v;
  }

  leaf(data) {
    return this.value(data, [], []);
  }

  add(a, b) {
    return this.value(a.data + b.data, [a, b], [1, 1]);
  }

  mul(a, b) {
    return this.value(a.data * b.data, [a, b], [b.data, a.data]);
  }

  pow(a, p) {
    return this.value(Math.pow(a.data, p), [a], [p * Math.pow(a.data, p - 1)]);
  }

  log(a) {
    return this.value(Math.log(a.data), [a], [1 / a.data]);
  }

  exp(a) {
    return this.value(Math.exp(a.data), [a], [Math.exp(a.data)]);
  }

  relu(a) {
    return this.value(Math.max(0, a.data), [a], [a.data > 0 ? 1 : 0]);
  }

  // Python's operators wrap a bare number in a `Value` first, so the graph
  // carries a leaf for every constant. These keep that shape.
  addNum(a, c) {
    return this.add(a, this.leaf(c));
  }

  mulNum(a, c) {
    return this.mul(a, this.leaf(c));
  }

  subNum(a, c) {
    return this.addNum(a, -c);
  }

  neg(a) {
    return this.mulNum(a, -1);
  }

  // `a / b` is `a * b**-1`, as `__truediv__` has it.
  div(a, b) {
    return this.mul(a, this.pow(b, -1));
  }

  divNum(a, c) {
    return this.mulNum(a, Math.pow(c, -1));
  }

  // `sum(xs)` starts at 0, so Python's graph carries that leaf too.
  sum(xs) {
    let acc = this.leaf(0);
    for (let i = 0; i < xs.length; i++) acc = this.add(acc, xs[i]);
    return acc;
  }

  // Everything above this length is one training step's graph, dropped whole.
  rewind(len) {
    this.nodes.length = len;
  }
}

function buildTopo(v, visited, topo) {
  if (visited[v.id]) return;
  visited[v.id] = 1;
  for (let i = 0; i < v.children.length; i++)
    buildTopo(v.children[i], visited, topo);
  topo.push(v);
}

function backward(root, nodeCount) {
  const topo = [];
  const visited = new Uint8Array(nodeCount);
  buildTopo(root, visited, topo);

  root.grad = 1;
  for (let i = topo.length - 1; i >= 0; i--) {
    const v = topo[i];
    for (let j = 0; j < v.children.length; j++) {
      v.children[j].grad += v.localGrads[j] * v.grad;
    }
  }
}

// Let there be a Tokenizer to translate strings to sequences of integers
// ("tokens") and back.
class Tokenizer {
  constructor(docs) {
    const seen = new Set();
    for (const doc of docs) for (const c of doc) seen.add(c);
    this.uchars = [...seen].sort();
    this.bos = this.uchars.length;
    this.vocabSize = this.bos + 1;
  }

  index(c) {
    for (let i = 0; i < this.uchars.length; i++) {
      if (this.uchars[i] === c) return i;
    }
    throw new Error(`no token for ${c}`);
  }

  // Surround the document with the BOS token on both sides.
  encode(doc) {
    const tokens = [this.bos];
    for (const c of doc) tokens.push(this.index(c));
    tokens.push(this.bos);
    return tokens;
  }
}

// Initialize the parameters, to store the knowledge of the model.
const N_LAYER = 1;
const N_EMBD = 16;
const BLOCK_SIZE = 16;
const N_HEAD = 4;
const HEAD_DIM = N_EMBD / N_HEAD;

function matrix(g, rng, nout, nin, std = 0.08) {
  const rows = [];
  for (let o = 0; o < nout; o++) {
    const row = [];
    for (let i = 0; i < nin; i++) row.push(g.leaf(rng.gauss(std)));
    rows.push(row);
  }
  return rows;
}

function stateDict(g, rng, vocabSize) {
  const state = {
    wte: matrix(g, rng, vocabSize, N_EMBD),
    wpe: matrix(g, rng, BLOCK_SIZE, N_EMBD),
    lmHead: matrix(g, rng, vocabSize, N_EMBD),
    layers: [],
  };
  for (let i = 0; i < N_LAYER; i++) {
    state.layers.push({
      attnWq: matrix(g, rng, N_EMBD, N_EMBD),
      attnWk: matrix(g, rng, N_EMBD, N_EMBD),
      attnWv: matrix(g, rng, N_EMBD, N_EMBD),
      attnWo: matrix(g, rng, N_EMBD, N_EMBD),
      mlpFc1: matrix(g, rng, 4 * N_EMBD, N_EMBD),
      mlpFc2: matrix(g, rng, N_EMBD, 4 * N_EMBD),
    });
  }
  return state;
}

// Flatten every matrix into a single list of parameters.
function params(state) {
  const out = [];
  const flatten = (m) => {
    for (const row of m) for (const p of row) out.push(p);
  };
  flatten(state.wte);
  flatten(state.wpe);
  flatten(state.lmHead);
  for (const layer of state.layers) {
    flatten(layer.attnWq);
    flatten(layer.attnWk);
    flatten(layer.attnWv);
    flatten(layer.attnWo);
    flatten(layer.mlpFc1);
    flatten(layer.mlpFc2);
  }
  return out;
}

// Define the model architecture: a function mapping tokens and parameters to
// logits over what comes next. Follow GPT-2, blessed among the GPTs, with minor
// differences: layernorm -> rmsnorm, no biases, GeLU -> ReLU.
function linear(g, x, w) {
  const out = [];
  for (const row of w) {
    const terms = [];
    for (let i = 0; i < row.length; i++) terms.push(g.mul(row[i], x[i]));
    out.push(g.sum(terms));
  }
  return out;
}

function softmax(g, logits) {
  let maxVal = -Infinity;
  for (const val of logits) maxVal = Math.max(maxVal, val.data);
  const exps = logits.map((l) => g.exp(g.subNum(l, maxVal)));
  const total = g.sum(exps);
  return exps.map((e) => g.div(e, total));
}

function rmsnorm(g, x) {
  const squares = x.map((xi) => g.mul(xi, xi));
  const ms = g.divNum(g.sum(squares), x.length);
  const scale = g.pow(g.addNum(ms, 1e-5), -0.5);
  return x.map((xi) => g.mul(xi, scale));
}

function addVec(g, a, b) {
  return a.map((ai, i) => g.add(ai, b[i]));
}

function gpt(g, state, tokenId, posId, keys, values) {
  let x = addVec(g, state.wte[tokenId], state.wpe[posId]);
  // Not redundant: the backward pass reaches here via the residual connection.
  x = rmsnorm(g, x);

  for (let li = 0; li < N_LAYER; li++) {
    const layer = state.layers[li];

    // 1) Multi-head Attention block
    const xResidual = x;
    x = rmsnorm(g, x);
    const q = linear(g, x, layer.attnWq);
    keys[li].push(linear(g, x, layer.attnWk));
    values[li].push(linear(g, x, layer.attnWv));

    const xAttn = [];
    for (let h = 0; h < N_HEAD; h++) {
      const hs = h * HEAD_DIM;
      const layerKeys = keys[li];
      const layerValues = values[li];

      const attnLogits = [];
      for (let t = 0; t < layerKeys.length; t++) {
        const kt = layerKeys[t];
        const terms = [];
        for (let j = 0; j < HEAD_DIM; j++)
          terms.push(g.mul(q[hs + j], kt[hs + j]));
        attnLogits.push(g.divNum(g.sum(terms), Math.pow(HEAD_DIM, 0.5)));
      }
      const attnWeights = softmax(g, attnLogits);

      for (let j = 0; j < HEAD_DIM; j++) {
        const terms = [];
        for (let t = 0; t < layerValues.length; t++) {
          terms.push(g.mul(attnWeights[t], layerValues[t][hs + j]));
        }
        xAttn.push(g.sum(terms));
      }
    }
    x = addVec(g, linear(g, xAttn, layer.attnWo), xResidual);

    // 2) MLP block
    const mlpResidual = x;
    x = rmsnorm(g, x);
    x = linear(g, x, layer.mlpFc1);
    x = linear(
      g,
      x.map((xi) => g.relu(xi)),
      layer.mlpFc2,
    );
    x = addVec(g, x, mlpResidual);
  }

  return linear(g, x, state.lmHead);
}

function emptyCache() {
  return Array.from({ length: N_LAYER }, () => []);
}

// Forward the token sequence through the model, building up the computation
// graph all the way to the loss.
function forwardLoss(g, state, tokens) {
  const n = Math.min(BLOCK_SIZE, tokens.length - 1);
  const keys = emptyCache();
  const values = emptyCache();

  const losses = [];
  for (let posId = 0; posId < n; posId++) {
    const logits = gpt(g, state, tokens[posId], posId, keys, values);
    const probs = softmax(g, logits);
    losses.push(g.neg(g.log(probs[tokens[posId + 1]])));
  }
  // The average loss over the document sequence. May yours be low.
  return g.mulNum(g.sum(losses), 1 / n);
}

// Let there be Adam, the blessed optimizer and its buffers.
const LEARNING_RATE = 0.01;
const BETA1 = 0.85;
const BETA2 = 0.99;
const EPS_ADAM = 1e-8;

class Adam {
  constructor(nParams) {
    this.m = new Float64Array(nParams);
    this.v = new Float64Array(nParams);
  }

  step(ps, lr, step) {
    const bias1 = 1 - Math.pow(BETA1, step + 1);
    const bias2 = 1 - Math.pow(BETA2, step + 1);
    for (let i = 0; i < ps.length; i++) {
      const p = ps[i];
      this.m[i] = BETA1 * this.m[i] + (1 - BETA1) * p.grad;
      this.v[i] = BETA2 * this.v[i] + (1 - BETA2) * p.grad * p.grad;
      const mHat = this.m[i] / bias1;
      const vHat = this.v[i] / bias2;
      p.data -= (lr * mHat) / (Math.sqrt(vHat) + EPS_ADAM);
      p.grad = 0;
    }
  }
}

const DOCS = [
  "emma", "mary", "ada", "madeleine", "katelyn", "karla", "jayde", "harleigh",
  "paityn", "rosalina", "rita", "margaux", "mayra", "klara", "janiah", "coralie",
  "blythe", "joana", "minerva", "jalani", "isra", "zariya", "rosanna", "loxley",
  "emmanuelle", "renae", "eveline", "yazmine", "corinna", "makyla", "marilynn", "malea",
];

// One measured iteration: a fresh model, then `steps` of forward, backward, Adam.
function train(steps) {
  const rng = new Rng(SEED);
  const tok = new Tokenizer(DOCS);
  const g = new Graph();
  const state = stateDict(g, rng, tok.vocabSize);
  const nParams = g.len();
  const ps = params(state);
  const adam = new Adam(ps.length);

  let lossValue = 0;
  for (let step = 0; step < steps; step++) {
    const tokens = tok.encode(DOCS[step % DOCS.length]);
    const loss = forwardLoss(g, state, tokens);
    backward(loss, g.len());
    lossValue = loss.data;
    adam.step(ps, LEARNING_RATE * (1 - step / steps), step);
    g.rewind(nParams);
  }
  return lossValue;
}

// Unlike the example's `generate`, this never stops early on a BOS draw: the
// position count has to be fixed for the throughput figure to mean anything.
function generate(g, state, tok, rng) {
  const nParams = g.len();
  const keys = emptyCache();
  const values = emptyCache();

  let tokenId = tok.bos;
  let sample = "";
  for (let posId = 0; posId < BLOCK_SIZE; posId++) {
    const logits = gpt(g, state, tokenId, posId, keys, values);
    const scaled = logits.map((l) => g.divNum(l, TEMPERATURE));
    const probs = softmax(g, scaled);
    tokenId = rng.choice(probs.map((p) => p.data));
    if (tokenId !== tok.bos) sample += tok.uchars[tokenId];
  }
  g.rewind(nParams);
  return sample;
}

// Weights steer which token is drawn, never what a position costs, so an
// untrained model times the same as a trained one and the phase stands alone.
function infer(samples) {
  const rng = new Rng(SEED);
  const tok = new Tokenizer(DOCS);
  const g = new Graph();
  const state = stateDict(g, rng, tok.vocabSize);

  let last = "";
  for (let i = 0; i < samples; i++) last = generate(g, state, tok, rng);
  return last;
}

// Next-token predictions per iteration, which is what the throughput counts.
// A step count would move with the corpus's document lengths.
function trainTokens(tok, steps) {
  let total = 0;
  for (let step = 0; step < steps; step++) {
    total += Math.min(BLOCK_SIZE, tok.encode(DOCS[step % DOCS.length]).length - 1);
  }
  return total;
}

function measure(label, workPerIter, f) {
  for (let w = 0; w < WARMUP; w++) f();
  let result;
  const start = process.hrtime.bigint();
  for (let i = 0; i < ITERATIONS; i++) result = f();
  printThroughput(
    label,
    workPerIter,
    ITERATIONS,
    Number(process.hrtime.bigint() - start),
    "tokens",
  );
  return result;
}

console.log(
  `microgpt ${STEPS} training steps, ${SAMPLES} samples: ${ITERATIONS} iter (warmup ${WARMUP})`,
);

const loss = measure("train", trainTokens(new Tokenizer(DOCS), STEPS), () =>
  train(STEPS),
);
console.log(`final loss = ${loss.toFixed(6)}`);

const sample = measure("infer", SAMPLES * BLOCK_SIZE, () => infer(SAMPLES));
console.log(`last sample = ${sample}`);
