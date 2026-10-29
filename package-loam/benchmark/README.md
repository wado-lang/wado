# Loam Benchmarks

Loam's backends against onnxruntime, and on Gemma 3 against llama.cpp, in the
format of [`benchmark/`](../../benchmark/README.md): each arm prints a
throughput line, so `benchmark/pick.ts` and `benchmark/ab.ts` read the logs.

## Setup

The models are not in the repository. Fetch them:

```sh
package-loam/example/gpt2-124m/fetch.sh
package-loam/example/gemma3-270m/fetch.sh
```

The tasks fetch the rest themselves, each pinned by version and SHA-256 and
cached under `~/.cache/loam`:

- `fetch_onnxruntime.sh`: Microsoft's onnxruntime release.
- `fetch_llama_cpp.sh`: llama.cpp's Vulkan release.
- `gemma3_270m/fetch_gguf.sh`: Gemma 3 (270M) as GGUF, for llama.cpp.

## Tasks

```sh
cd package-loam/benchmark
mise run gpt2-124m     # GPT-2 (124M) greedy generation
mise run gemma3-270m   # Gemma 3 (270M) greedy generation
mise run all
```

Each arm continues "Hello, my name is" by 8 tokens, greedily, after one warmup
run, three times, and prints the tokens it generated, so a divergence between
arms is a bug rather than noise. The throughput counts generated tokens, and
`ms/iter` is one run of 8. The programs other than Loam's are the timing
`generate.wado` does, on each runtime's own API, linked against its release as
shipped.

## Environment

Wado 2026-10-24 (`-O2`), wasmtime 50.0.0, wgpu-core 29, onnxruntime 1.30.0,
llama.cpp b11846, gcc 15.2.0 (`-O2`), AMD Ryzen 7 5700X (8 cores, 16 threads),
NVIDIA GeForce RTX 4060 Ti (driver 595.99.02, Vulkan), Linux x86_64. Each
figure is the best of three runs.

## GPT-2 (124M)

Every arm recomputes the whole sequence at each token, since the graph takes no
key-value cache.

| Arm                              | Program                                    |
| -------------------------------- | ------------------------------------------ |
| onnxruntime, its own thread pool | `gpt2_124m/onnxruntime.cpp`                |
| onnxruntime, 1 thread            | `gpt2_124m/onnxruntime.cpp 1`              |
| Loam `WebGpuBackend`             | `gpt2_124m_webgpu.wado`, `wado run-webgpu` |
| Loam `Cpu`                       | `gpt2_124m.wado`, `wado run`               |

Every arm generated "John. I'm a writer, and".

| Implementation                   |      Throughput |     ms/iter | vs best |
| -------------------------------- | --------------: | ----------: | ------- |
| **Loam** `WebGpuBackend`         | 128.72 tokens/s |   62.150 ms | 1.00x   |
| onnxruntime, its own thread pool |  66.48 tokens/s |  120.339 ms | 1.94x   |
| onnxruntime, 1 thread            |  37.57 tokens/s |  212.931 ms | 3.43x   |
| **Loam** `Cpu`                   |   3.66 tokens/s | 2188.210 ms | 35.21x  |

## Gemma 3 (270M)

Each run starts from an empty attention cache: the first token reads the
prompt, and each one after it only itself against the cache. The Loam and
onnxruntime arms read the weights `example/gemma3-270m/fetch.sh` downloads, in
f32. llama.cpp
reads GGUF in F16, the precision nearest it. Google ships no F16 GGUF and
ggml-org only Q8_0, so it is unsloth's conversion.

| Arm                              | Program                                        |
| -------------------------------- | ---------------------------------------------- |
| onnxruntime, its own thread pool | `gemma3_270m/onnxruntime.cpp`                  |
| llama.cpp Vulkan                 | `gemma3_270m/llama_cpp.cpp`                    |
| Loam `WebGpuBackend`             | `gemma3_270m_webgpu.wado`, `wado run-webgpu`   |
| Loam `Cpu`                       | `gemma3_270m.wado`, `wado run --collector drc` |

Every arm generated "[Your Name] and I am a", the tokens the example's
`generate.json` records.

| Implementation                   |      Throughput |     ms/iter | vs best |
| -------------------------------- | --------------: | ----------: | ------- |
| llama.cpp Vulkan                 | 269.12 tokens/s |   29.726 ms | 1.00x   |
| **Loam** `WebGpuBackend`         | 246.02 tokens/s |   32.517 ms | 1.09x   |
| onnxruntime, its own thread pool |  35.88 tokens/s |  222.946 ms | 7.50x   |
| **Loam** `Cpu`                   |   6.80 tokens/s | 1177.114 ms | 39.58x  |
