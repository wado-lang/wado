// microgpt training and inference benchmark for Rust.
//
// A port of Andrej Karpathy's dependency-free Python microgpt.
// Copyright (c) Andrej Karpathy, MIT License.
// https://gist.github.com/karpathy/8627fe009c40f57531cb18360106ce95
//
// The same program as microgpt.wado, with one forced difference. Wado's
// `Graph::value` hands back a `&mut Value` into a list that keeps growing, and
// a node holds those handles as its children. Rust has no way to say that: a
// `&mut` into a `Vec` dies at the next push. So a node here is a `usize` index,
// children are indices, and every read spells out `self.nodes[i]`. That index
// is also the identity the `visited` set needs, which is why this file has no
// `id` field and microgpt.wado does.
//
// Both phases reseed, so every arm prints the same loss and the same sample and
// they can be checked against each other.
//
// How to run:
//   rustc -O --edition 2024 -o microgpt_rs microgpt.rs && ./microgpt_rs

use std::collections::BTreeSet;
use std::time::Instant;

const ITERATIONS: u64 = 3;
const WARMUP: u64 = 1;

fn print_throughput(label: &str, work_per_iter: f64, n: u64, elapsed_ns: u128, unit: &str) {
    let secs = elapsed_ns as f64 / 1e9;
    let rate = if secs > 0.0 {
        work_per_iter * n as f64 / secs
    } else {
        0.0
    };
    let per_ms = elapsed_ns as f64 / n as f64 / 1e6;
    let rbuf = if rate >= 1e9 {
        format!("{:.2} G {unit}/s", rate / 1e9)
    } else if rate >= 1e6 {
        format!("{:.2} M {unit}/s", rate / 1e6)
    } else if rate >= 1e3 {
        format!("{:.2} k {unit}/s", rate / 1e3)
    } else {
        format!("{rate:.2} {unit}/s")
    };
    println!("{label}: {rbuf}   ({per_ms:.3} ms/iter, {n} iter)");
}

// One step per document, so a measured iteration is one pass over the corpus.
const STEPS: usize = 32;
// Each sample runs the full attention window, deeper than any training step
// reaches on this corpus.
const SAMPLES: usize = 24;
const TEMPERATURE: f64 = 0.5;
// Python seeds Mersenne Twister, so this draws different numbers than the original.
const SEED: u64 = 42;

// Let there be a deterministic source of chaos. This is `core:prng`'s, which the
// Wado arm draws from: SplitMix64 expands the seed, then xoshiro256++ runs.
fn splitmix(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

struct Rng {
    s: [u64; 4],
}

impl Rng {
    fn new(seed: u64) -> Rng {
        let mut state = seed;
        let s0 = splitmix(&mut state);
        let s1 = splitmix(&mut state);
        let s2 = splitmix(&mut state);
        let s3 = splitmix(&mut state);
        Rng { s: [s0, s1, s2, s3] }
    }

    fn next_u64(&mut self) -> u64 {
        let result = self.s[0].wrapping_add(self.s[3]).rotate_left(23).wrapping_add(self.s[0]);
        let t = self.s[1] << 17;
        self.s[2] ^= self.s[0];
        self.s[3] ^= self.s[1];
        self.s[1] ^= self.s[2];
        self.s[0] ^= self.s[3];
        self.s[2] ^= t;
        self.s[3] = self.s[3].rotate_left(45);
        result
    }

    // 53 bits, the mantissa of an f64, scaled into [0, 1).
    fn random(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / 9007199254740992.0
    }

    fn gauss(&mut self, std: f64) -> f64 {
        let mut u = self.random();
        while u <= 0.0 {
            u = self.random();
        }
        let v = self.random();
        std * (-2.0 * u.ln()).sqrt() * (2.0 * std::f64::consts::PI * v).cos()
    }

    // `random.choices(range(n), weights)`: one draw against the cumulative sum.
    fn choice(&mut self, weights: &[f64]) -> usize {
        let mut total = 0.0;
        for &w in weights {
            total += w;
        }
        let mut r = self.random() * total;
        for (i, &w) in weights.iter().enumerate() {
            r -= w;
            if r < 0.0 {
                return i;
            }
        }
        weights.len() - 1
    }
}

// Let there be Autograd to recursively apply the chain rule through a
// computation graph. `Graph` is the heap those nodes live in, and a node's
// index in it is both its handle and its identity.
struct Value {
    data: f64,
    grad: f64,
    children: Vec<usize>,
    local_grads: Vec<f64>,
}

struct Graph {
    nodes: Vec<Value>,
}

impl Graph {
    fn new() -> Graph {
        Graph { nodes: Vec::new() }
    }

    fn len(&self) -> usize {
        self.nodes.len()
    }

    fn value(&mut self, data: f64, children: Vec<usize>, local_grads: Vec<f64>) -> usize {
        self.nodes.push(Value {
            data,
            grad: 0.0,
            children,
            local_grads,
        });
        self.nodes.len() - 1
    }

    fn leaf(&mut self, data: f64) -> usize {
        self.value(data, Vec::new(), Vec::new())
    }

    fn add(&mut self, a: usize, b: usize) -> usize {
        let data = self.nodes[a].data + self.nodes[b].data;
        self.value(data, vec![a, b], vec![1.0, 1.0])
    }

    fn mul(&mut self, a: usize, b: usize) -> usize {
        let (da, db) = (self.nodes[a].data, self.nodes[b].data);
        self.value(da * db, vec![a, b], vec![db, da])
    }

    fn pow(&mut self, a: usize, p: f64) -> usize {
        let da = self.nodes[a].data;
        self.value(da.powf(p), vec![a], vec![p * da.powf(p - 1.0)])
    }

    fn log(&mut self, a: usize) -> usize {
        let da = self.nodes[a].data;
        self.value(da.ln(), vec![a], vec![1.0 / da])
    }

    fn exp(&mut self, a: usize) -> usize {
        let da = self.nodes[a].data;
        self.value(da.exp(), vec![a], vec![da.exp()])
    }

    fn relu(&mut self, a: usize) -> usize {
        let da = self.nodes[a].data;
        let slope = if da > 0.0 { 1.0 } else { 0.0 };
        self.value(da.max(0.0), vec![a], vec![slope])
    }

    // Python's operators wrap a bare number in a `Value` first, so the graph
    // carries a leaf for every constant. These keep that shape.
    fn add_num(&mut self, a: usize, c: f64) -> usize {
        let n = self.leaf(c);
        self.add(a, n)
    }

    fn mul_num(&mut self, a: usize, c: f64) -> usize {
        let n = self.leaf(c);
        self.mul(a, n)
    }

    fn sub_num(&mut self, a: usize, c: f64) -> usize {
        self.add_num(a, -c)
    }

    fn neg(&mut self, a: usize) -> usize {
        self.mul_num(a, -1.0)
    }

    // `a / b` is `a * b**-1`, as `__truediv__` has it.
    fn div(&mut self, a: usize, b: usize) -> usize {
        let inv = self.pow(b, -1.0);
        self.mul(a, inv)
    }

    fn div_num(&mut self, a: usize, c: f64) -> usize {
        self.mul_num(a, c.powf(-1.0))
    }

    // `sum(xs)` starts at 0, so Python's graph carries that leaf too.
    fn sum(&mut self, xs: &[usize]) -> usize {
        let mut acc = self.leaf(0.0);
        for &x in xs {
            acc = self.add(acc, x);
        }
        acc
    }

    // Everything above this length is one training step's graph, dropped whole.
    fn rewind(&mut self, len: usize) {
        self.nodes.truncate(len);
    }
}

fn build_topo(nodes: &[Value], v: usize, visited: &mut [bool], topo: &mut Vec<usize>) {
    if visited[v] {
        return;
    }
    visited[v] = true;
    for i in 0..nodes[v].children.len() {
        build_topo(nodes, nodes[v].children[i], visited, topo);
    }
    topo.push(v);
}

fn backward(g: &mut Graph, root: usize) {
    let mut topo: Vec<usize> = Vec::new();
    let mut visited = vec![false; g.len()];
    build_topo(&g.nodes, root, &mut visited, &mut topo);

    g.nodes[root].grad = 1.0;
    for &v in topo.iter().rev() {
        // Two disjoint borrows of `g.nodes`, so the values come out first.
        let grad = g.nodes[v].grad;
        for j in 0..g.nodes[v].children.len() {
            let (child, local) = (g.nodes[v].children[j], g.nodes[v].local_grads[j]);
            g.nodes[child].grad += local * grad;
        }
    }
}

// Let there be a Tokenizer to translate strings to sequences of integers
// ("tokens") and back.
struct Tokenizer {
    uchars: Vec<char>,
    bos: usize,
    vocab_size: usize,
}

impl Tokenizer {
    fn new(docs: &[&str]) -> Tokenizer {
        let mut seen = BTreeSet::new();
        for doc in docs {
            for c in doc.chars() {
                seen.insert(c);
            }
        }
        let uchars: Vec<char> = seen.into_iter().collect();
        let bos = uchars.len();
        Tokenizer {
            uchars,
            bos,
            vocab_size: bos + 1,
        }
    }

    fn index(&self, c: char) -> usize {
        for (i, &u) in self.uchars.iter().enumerate() {
            if u == c {
                return i;
            }
        }
        panic!("no token for {c}");
    }

    // Surround the document with the BOS token on both sides.
    fn encode(&self, doc: &str) -> Vec<usize> {
        let mut tokens = vec![self.bos];
        for c in doc.chars() {
            tokens.push(self.index(c));
        }
        tokens.push(self.bos);
        tokens
    }
}

// Initialize the parameters, to store the knowledge of the model.
const N_LAYER: usize = 1;
const N_EMBD: usize = 16;
const BLOCK_SIZE: usize = 16;
const N_HEAD: usize = 4;
const HEAD_DIM: usize = N_EMBD / N_HEAD;

fn matrix(g: &mut Graph, rng: &mut Rng, nout: usize, nin: usize) -> Vec<Vec<usize>> {
    let std = 0.08;
    let mut rows = Vec::with_capacity(nout);
    for _ in 0..nout {
        let mut row = Vec::with_capacity(nin);
        for _ in 0..nin {
            let d = rng.gauss(std);
            row.push(g.leaf(d));
        }
        rows.push(row);
    }
    rows
}

// Python keys these by string in one dict; the shapes are fixed, so name them.
struct Layer {
    attn_wq: Vec<Vec<usize>>,
    attn_wk: Vec<Vec<usize>>,
    attn_wv: Vec<Vec<usize>>,
    attn_wo: Vec<Vec<usize>>,
    mlp_fc1: Vec<Vec<usize>>,
    mlp_fc2: Vec<Vec<usize>>,
}

struct StateDict {
    wte: Vec<Vec<usize>>,
    wpe: Vec<Vec<usize>>,
    lm_head: Vec<Vec<usize>>,
    layers: Vec<Layer>,
}

impl StateDict {
    fn new(g: &mut Graph, rng: &mut Rng, vocab_size: usize) -> StateDict {
        let wte = matrix(g, rng, vocab_size, N_EMBD);
        let wpe = matrix(g, rng, BLOCK_SIZE, N_EMBD);
        let lm_head = matrix(g, rng, vocab_size, N_EMBD);
        let mut layers = Vec::with_capacity(N_LAYER);
        for _ in 0..N_LAYER {
            layers.push(Layer {
                attn_wq: matrix(g, rng, N_EMBD, N_EMBD),
                attn_wk: matrix(g, rng, N_EMBD, N_EMBD),
                attn_wv: matrix(g, rng, N_EMBD, N_EMBD),
                attn_wo: matrix(g, rng, N_EMBD, N_EMBD),
                mlp_fc1: matrix(g, rng, 4 * N_EMBD, N_EMBD),
                mlp_fc2: matrix(g, rng, N_EMBD, 4 * N_EMBD),
            });
        }
        StateDict {
            wte,
            wpe,
            lm_head,
            layers,
        }
    }

    // Flatten every matrix into a single list of parameters.
    fn params(&self) -> Vec<usize> {
        let mut out = Vec::new();
        let mut flatten = |m: &Vec<Vec<usize>>| {
            for row in m {
                out.extend_from_slice(row);
            }
        };
        flatten(&self.wte);
        flatten(&self.wpe);
        flatten(&self.lm_head);
        for layer in &self.layers {
            flatten(&layer.attn_wq);
            flatten(&layer.attn_wk);
            flatten(&layer.attn_wv);
            flatten(&layer.attn_wo);
            flatten(&layer.mlp_fc1);
            flatten(&layer.mlp_fc2);
        }
        out
    }
}

// Define the model architecture: a function mapping tokens and parameters to
// logits over what comes next. Follow GPT-2, blessed among the GPTs, with minor
// differences: layernorm -> rmsnorm, no biases, GeLU -> ReLU.
fn linear(g: &mut Graph, x: &[usize], w: &[Vec<usize>]) -> Vec<usize> {
    let mut out = Vec::with_capacity(w.len());
    for row in w {
        let terms: Vec<usize> = (0..row.len()).map(|i| g.mul(row[i], x[i])).collect();
        out.push(g.sum(&terms));
    }
    out
}

fn softmax(g: &mut Graph, logits: &[usize]) -> Vec<usize> {
    let max_val = logits
        .iter()
        .fold(f64::NEG_INFINITY, |acc, &l| acc.max(g.nodes[l].data));
    let exps: Vec<usize> = logits
        .iter()
        .map(|&l| {
            let shifted = g.sub_num(l, max_val);
            g.exp(shifted)
        })
        .collect();
    let total = g.sum(&exps);
    exps.iter().map(|&e| g.div(e, total)).collect()
}

fn rmsnorm(g: &mut Graph, x: &[usize]) -> Vec<usize> {
    let squares: Vec<usize> = x.iter().map(|&xi| g.mul(xi, xi)).collect();
    let sum_sq = g.sum(&squares);
    let ms = g.div_num(sum_sq, x.len() as f64);
    let shifted = g.add_num(ms, 1e-5);
    let scale = g.pow(shifted, -0.5);
    x.iter().map(|&xi| g.mul(xi, scale)).collect()
}

fn add_vec(g: &mut Graph, a: &[usize], b: &[usize]) -> Vec<usize> {
    assert_eq!(a.len(), b.len());
    (0..a.len()).map(|i| g.add(a[i], b[i])).collect()
}

fn gpt(
    g: &mut Graph,
    state: &StateDict,
    token_id: usize,
    pos_id: usize,
    keys: &mut [Vec<Vec<usize>>],
    values: &mut [Vec<Vec<usize>>],
) -> Vec<usize> {
    let mut x = add_vec(g, &state.wte[token_id], &state.wpe[pos_id]);
    // Not redundant: the backward pass reaches here via the residual connection.
    x = rmsnorm(g, &x);

    for li in 0..N_LAYER {
        let layer = &state.layers[li];

        // 1) Multi-head Attention block
        let x_residual = x.clone();
        x = rmsnorm(g, &x);
        let q = linear(g, &x, &layer.attn_wq);
        let k = linear(g, &x, &layer.attn_wk);
        let v = linear(g, &x, &layer.attn_wv);
        keys[li].push(k);
        values[li].push(v);

        let mut x_attn = Vec::with_capacity(N_EMBD);
        for h in 0..N_HEAD {
            let hs = h * HEAD_DIM;

            let mut attn_logits = Vec::with_capacity(keys[li].len());
            for t in 0..keys[li].len() {
                let terms: Vec<usize> = (0..HEAD_DIM)
                    .map(|j| {
                        let kt = keys[li][t][hs + j];
                        g.mul(q[hs + j], kt)
                    })
                    .collect();
                let score = g.sum(&terms);
                attn_logits.push(g.div_num(score, (HEAD_DIM as f64).powf(0.5)));
            }
            let attn_weights = softmax(g, &attn_logits);

            for j in 0..HEAD_DIM {
                let mut terms = Vec::with_capacity(values[li].len());
                for t in 0..values[li].len() {
                    let vt = values[li][t][hs + j];
                    terms.push(g.mul(attn_weights[t], vt));
                }
                x_attn.push(g.sum(&terms));
            }
        }
        x = linear(g, &x_attn, &layer.attn_wo);
        x = add_vec(g, &x, &x_residual);

        // 2) MLP block
        let mlp_residual = x.clone();
        x = rmsnorm(g, &x);
        x = linear(g, &x, &layer.mlp_fc1);
        let activated: Vec<usize> = x.iter().map(|&xi| g.relu(xi)).collect();
        x = linear(g, &activated, &layer.mlp_fc2);
        x = add_vec(g, &x, &mlp_residual);
    }

    linear(g, &x, &state.lm_head)
}

fn empty_cache() -> Vec<Vec<Vec<usize>>> {
    (0..N_LAYER).map(|_| Vec::new()).collect()
}

// Forward the token sequence through the model, building up the computation
// graph all the way to the loss.
fn forward_loss(g: &mut Graph, state: &StateDict, tokens: &[usize]) -> usize {
    let n = BLOCK_SIZE.min(tokens.len() - 1);
    let mut keys = empty_cache();
    let mut values = empty_cache();

    let mut losses = Vec::with_capacity(n);
    for pos_id in 0..n {
        let logits = gpt(g, state, tokens[pos_id], pos_id, &mut keys, &mut values);
        let probs = softmax(g, &logits);
        let log_prob = g.log(probs[tokens[pos_id + 1]]);
        losses.push(g.neg(log_prob));
    }
    // The average loss over the document sequence. May yours be low.
    let total = g.sum(&losses);
    g.mul_num(total, 1.0 / n as f64)
}

// Let there be Adam, the blessed optimizer and its buffers.
const LEARNING_RATE: f64 = 0.01;
const BETA1: f64 = 0.85;
const BETA2: f64 = 0.99;
const EPS_ADAM: f64 = 1e-8;

struct Adam {
    m: Vec<f64>,
    v: Vec<f64>,
}

impl Adam {
    fn new(n_params: usize) -> Adam {
        Adam {
            m: vec![0.0; n_params],
            v: vec![0.0; n_params],
        }
    }

    fn step(&mut self, g: &mut Graph, params: &[usize], lr: f64, step: usize) {
        let bias1 = 1.0 - BETA1.powf((step + 1) as f64);
        let bias2 = 1.0 - BETA2.powf((step + 1) as f64);
        for (i, &pi) in params.iter().enumerate() {
            let p = &mut g.nodes[pi];
            self.m[i] = BETA1 * self.m[i] + (1.0 - BETA1) * p.grad;
            self.v[i] = BETA2 * self.v[i] + (1.0 - BETA2) * p.grad * p.grad;
            let m_hat = self.m[i] / bias1;
            let v_hat = self.v[i] / bias2;
            p.data -= lr * m_hat / (v_hat.sqrt() + EPS_ADAM);
            p.grad = 0.0;
        }
    }
}

#[rustfmt::skip]
const DOCS: [&str; 32] = [
    "emma", "mary", "ada", "madeleine", "katelyn", "karla", "jayde", "harleigh",
    "paityn", "rosalina", "rita", "margaux", "mayra", "klara", "janiah", "coralie",
    "blythe", "joana", "minerva", "jalani", "isra", "zariya", "rosanna", "loxley",
    "emmanuelle", "renae", "eveline", "yazmine", "corinna", "makyla", "marilynn", "malea",
];

// One measured iteration: a fresh model, then `steps` of forward, backward, Adam.
fn train(steps: usize) -> f64 {
    let mut rng = Rng::new(SEED);
    let tok = Tokenizer::new(&DOCS);
    let mut g = Graph::new();
    let state = StateDict::new(&mut g, &mut rng, tok.vocab_size);
    let n_params = g.len();
    let params = state.params();
    let mut adam = Adam::new(params.len());

    let mut loss_value = 0.0;
    for step in 0..steps {
        let tokens = tok.encode(DOCS[step % DOCS.len()]);
        let loss = forward_loss(&mut g, &state, &tokens);
        backward(&mut g, loss);
        loss_value = g.nodes[loss].data;
        adam.step(
            &mut g,
            &params,
            LEARNING_RATE * (1.0 - step as f64 / steps as f64),
            step,
        );
        g.rewind(n_params);
    }
    loss_value
}

// Unlike the example's `generate`, this never stops early on a BOS draw: the
// position count has to be fixed for the throughput figure to mean anything.
fn generate(g: &mut Graph, state: &StateDict, tok: &Tokenizer, rng: &mut Rng) -> String {
    let n_params = g.len();
    let mut keys = empty_cache();
    let mut values = empty_cache();

    let mut token_id = tok.bos;
    let mut sample = String::new();
    for pos_id in 0..BLOCK_SIZE {
        let logits = gpt(g, state, token_id, pos_id, &mut keys, &mut values);
        let scaled: Vec<usize> = logits.iter().map(|&l| g.div_num(l, TEMPERATURE)).collect();
        let probs = softmax(g, &scaled);
        let weights: Vec<f64> = probs.iter().map(|&p| g.nodes[p].data).collect();
        token_id = rng.choice(&weights);
        if token_id != tok.bos {
            sample.push(tok.uchars[token_id]);
        }
    }
    g.rewind(n_params);
    sample
}

// Weights steer which token is drawn, never what a position costs, so an
// untrained model times the same as a trained one and the phase stands alone.
fn infer(samples: usize) -> String {
    let mut rng = Rng::new(SEED);
    let tok = Tokenizer::new(&DOCS);
    let mut g = Graph::new();
    let state = StateDict::new(&mut g, &mut rng, tok.vocab_size);

    let mut last = String::new();
    for _ in 0..samples {
        last = generate(&mut g, &state, &tok, &mut rng);
    }
    last
}

// Next-token predictions per iteration, which is what the throughput counts.
// A step count would move with the corpus's document lengths.
fn train_tokens(tok: &Tokenizer, steps: usize) -> f64 {
    let mut total = 0;
    for step in 0..steps {
        total += BLOCK_SIZE.min(tok.encode(DOCS[step % DOCS.len()]).len() - 1);
    }
    total as f64
}

fn measure<T>(label: &str, work_per_iter: f64, mut f: impl FnMut() -> T) -> T {
    for _ in 0..WARMUP {
        f();
    }
    let start = Instant::now();
    let mut result = f();
    for _ in 1..ITERATIONS {
        result = f();
    }
    print_throughput(label, work_per_iter, ITERATIONS, start.elapsed().as_nanos(), "tokens");
    result
}

fn main() {
    println!("microgpt {STEPS} training steps, {SAMPLES} samples: {ITERATIONS} iter (warmup {WARMUP})");

    let tok = Tokenizer::new(&DOCS);
    let loss = measure("train", train_tokens(&tok, STEPS), || train(STEPS));
    println!("final loss = {loss:.6}");

    let sample = measure("infer", (SAMPLES * BLOCK_SIZE) as f64, || infer(SAMPLES));
    println!("last sample = {sample}");
}
