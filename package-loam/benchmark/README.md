# Loam Benchmarks

Loam's backends against onnxruntime, in the format of
[`benchmark/`](../../benchmark/README.md): each arm prints a throughput line, so
`benchmark/pick.ts` and `benchmark/ab.ts` read the logs.

## Setup

The models are not in the repository. Fetch them and install onnxruntime:

```sh
package-loam/example/gpt2-124m/fetch.sh
mise run loam-oracle-deps
```

## Tasks

```sh
cd package-loam/benchmark
mise run gpt2-124m   # GPT-2 (124M) greedy generation
mise run all
```

## GPT-2 (124M)

`gpt2_124m/` continues "Hello, my name is" by 8 tokens, greedily, after one
warmup run, three times. Each arm prints the tokens it generated, so a
divergence between them is a bug rather than noise. The throughput counts
generated tokens.

| Arm                              | Program                                    |
| -------------------------------- | ------------------------------------------ |
| onnxruntime, its own thread pool | `onnxruntime.mjs`                          |
| onnxruntime, 1 thread            | `onnxruntime.mjs 1`                        |
| onnxruntime, WebGPU EP           | `onnxruntime.mjs webgpu`                   |
| Loam `WebGpuBackend`             | `gpt2_124m_webgpu.wado`, `wado run-webgpu` |
| Loam `Cpu`                       | `gpt2_124m.wado`, `wado run`               |

`generate.wado` is the timing both Loam arms share. Every arm recomputes the
whole sequence at each token, since the graph takes no key-value cache.

### Results

Environment: Wado 2026-10-24 (`-O2`), wasmtime 50.0.0, wgpu-core 29,
onnxruntime-node 1.30.0, Node.js v26.10.0, AMD Ryzen 7 5700X (8 cores, 16
threads), NVIDIA GeForce RTX 4060 Ti (driver 595.99.02), Linux x86_64. Best of
three runs. Every arm generated the same 8 tokens, "John. I'm a writer, and".

| Implementation                   |      Throughput |     ms/iter | vs best |
| -------------------------------- | --------------: | ----------: | ------- |
| **Loam** `WebGpuBackend`         | 129.30 tokens/s |   61.870 ms | 1.00x   |
| onnxruntime, its own thread pool |  70.72 tokens/s |  113.119 ms | 1.83x   |
| onnxruntime, WebGPU EP           |  60.22 tokens/s |  132.857 ms | 2.15x   |
| onnxruntime, 1 thread            |  37.26 tokens/s |  214.726 ms | 3.47x   |
| **Loam** `Cpu`                   |   3.67 tokens/s | 2178.337 ms | 35.21x  |

`ms/iter` is one run: 8 tokens generated after the prompt.
