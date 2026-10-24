# Loam Benchmarks

Loam's backends against onnxruntime, in the format of
[`benchmark/`](../../benchmark/README.md): each arm prints a throughput line, so
`benchmark/pick.ts` and `benchmark/ab.ts` read the logs.

## Setup

The models are not in the repository. Fetch them and install onnxruntime for
Node. The task fetches Microsoft's native onnxruntime release itself
(`gpt2_124m/fetch_onnxruntime.sh`, cached under `~/.cache/loam`):

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

| Arm                                      | Program                                    |
| ---------------------------------------- | ------------------------------------------ |
| onnxruntime-node, its own thread pool    | `onnxruntime.mjs`                          |
| onnxruntime-node, 1 thread               | `onnxruntime.mjs 1`                        |
| onnxruntime C++ API, its own thread pool | `onnxruntime.cpp`                          |
| onnxruntime C++ API, 1 thread            | `onnxruntime.cpp 1`                        |
| onnxruntime-node, WebGPU EP              | `onnxruntime.mjs webgpu`                   |
| Loam `WebGpuBackend`                     | `gpt2_124m_webgpu.wado`, `wado run-webgpu` |
| Loam `Cpu`                               | `gpt2_124m.wado`, `wado run`               |

`generate.wado` is the timing both Loam arms share. Every arm recomputes the
whole sequence at each token, since the graph takes no key-value cache.

`onnxruntime.cpp` is `onnxruntime.mjs` on onnxruntime's own C++ API, linked
against Microsoft's release as shipped. Both run onnxruntime 1.30.0, so the
pair prices what Node adds around `session.run`.

### Results

Environment: Wado 2026-10-24 (`-O2`), wasmtime 50.0.0, wgpu-core 29,
onnxruntime 1.30.0 (onnxruntime-node and Microsoft's release), Node.js
v26.10.0, gcc 15.2.0 (`-O2`), AMD Ryzen 7 5700X (8 cores, 16 threads), NVIDIA
GeForce RTX 4060 Ti (driver 595.99.02), Linux x86_64. Best of three runs. Every
arm generated the same 8 tokens, "John. I'm a writer, and".

| Implementation                           |      Throughput |     ms/iter | vs best |
| ---------------------------------------- | --------------: | ----------: | ------- |
| **Loam** `WebGpuBackend`                 | 128.81 tokens/s |   62.106 ms | 1.00x   |
| onnxruntime-node, its own thread pool    |  70.43 tokens/s |  113.581 ms | 1.83x   |
| onnxruntime C++ API, its own thread pool |  66.61 tokens/s |  120.106 ms | 1.93x   |
| onnxruntime-node, WebGPU EP              |  59.92 tokens/s |  133.517 ms | 2.15x   |
| onnxruntime C++ API, 1 thread            |  37.40 tokens/s |  213.895 ms | 3.44x   |
| onnxruntime-node, 1 thread               |  37.17 tokens/s |  215.236 ms | 3.47x   |
| **Loam** `Cpu`                           |   3.66 tokens/s | 2185.415 ms | 35.19x  |

`ms/iter` is one run: 8 tokens generated after the prompt.
