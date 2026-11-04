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

- `fetch_onnxruntime.sh`: Microsoft's onnxruntime release for the CPU, which
  ships no GPU provider.
- `fetch_llama_cpp.sh`: llama.cpp's Vulkan release.
- `gemma3_270m/fetch_gguf.sh`: Gemma 3 (270M) as GGUF, for llama.cpp.

The WebGPU arm of onnxruntime runs onnxruntime-node, which
`mise run loam-oracle-deps` installs as `package-lock.json` pins it. Without
it the arm is skipped.

## Tasks

```sh
cd package-loam/benchmark
mise run gpt2-124m     # GPT-2 (124M) greedy generation
mise run gemma3-270m   # Gemma 3 (270M) greedy generation
mise run all
```

Each arm continues "Hello, my name is" by 8 tokens, greedily, after one warmup
run, three times, and prints the tokens it generated, so a divergence between
arms is a bug rather than noise. Each run starts from an empty attention cache:
the first token reads the prompt, and each one after it only itself against
the cache. The throughput counts generated tokens, and `ms/iter` is one run of
8. The programs other than Loam's are the timing `generate.wado` does, on each
runtime's own API, linked against its release as shipped. The C++ onnxruntime
arms share `decoder.hpp`. The WebGPU one, `onnxruntime_webgpu.mjs`, decodes as
the examples' oracle does (`tools/greedy.mjs`): the cache stays on the device,
and only the logits come back.

## Environment

Wado 2026-11-04 (`-O2`), wasmtime 50.0.1, wgpu-core 29, onnxruntime 1.30.0,
onnxruntime-node 1.30.0 on Node 26.13.0, llama.cpp b11846, gcc 15.2.0 (`-O2`), AMD Ryzen 7 5700X (8 cores, 16 threads),
NVIDIA GeForce RTX 4060 Ti (driver 595.99.02, Vulkan), Linux x86_64. `wado`
and `wado-run-webgpu` are release builds, as the tasks build them. Each figure
is the best of three runs.

## GPT-2 (124M)

| Arm                              | Program                                    |
| -------------------------------- | ------------------------------------------ |
| onnxruntime, its own thread pool | `gpt2_124m/onnxruntime.cpp`                |
| onnxruntime, 1 thread            | `gpt2_124m/onnxruntime.cpp 1`              |
| onnxruntime WebGPU               | `onnxruntime_webgpu.mjs`, Node             |
| Loam `WebGpuBackend`             | `gpt2_124m_webgpu.wado`, `wado run-webgpu` |
| Loam `Cpu`                       | `gpt2_124m.wado`, `wado run`               |

Every arm generated "John. I'm a writer, and".

| Implementation                   |      Throughput |     ms/iter | vs best |
| -------------------------------- | --------------: | ----------: | ------- |
| **Loam** `WebGpuBackend`         | 248.22 tokens/s |   32.229 ms | 1.00x   |
| onnxruntime, its own thread pool |  72.34 tokens/s |  110.586 ms | 3.43x   |
| onnxruntime, 1 thread            |  60.10 tokens/s |  133.113 ms | 4.13x   |
| onnxruntime WebGPU               |  54.53 tokens/s |  146.701 ms | 4.55x   |
| **Loam** `Cpu`                   |   5.41 tokens/s | 1479.135 ms | 45.89x  |

## Gemma 3 (270M)

The Loam and onnxruntime arms read the weights `example/gemma3-270m/fetch.sh`
downloads, in f32. llama.cpp reads GGUF in F16, the precision nearest it. Google ships no F16 GGUF and
ggml-org only Q8_0, so it is unsloth's conversion.

| Arm                              | Program                                        |
| -------------------------------- | ---------------------------------------------- |
| onnxruntime, its own thread pool | `gemma3_270m/onnxruntime.cpp`                  |
| onnxruntime WebGPU               | `onnxruntime_webgpu.mjs`, Node                 |
| llama.cpp Vulkan                 | `gemma3_270m/llama_cpp.cpp`                    |
| Loam `WebGpuBackend`             | `gemma3_270m_webgpu.wado`, `wado run-webgpu`   |
| Loam `Cpu`                       | `gemma3_270m.wado`, `wado run --collector drc` |

Every arm generated "[Your Name] and I am a", the tokens the example's
`generate.json` records.

| Implementation                   |      Throughput |     ms/iter | vs best |
| -------------------------------- | --------------: | ----------: | ------- |
| llama.cpp Vulkan                 | 272.57 tokens/s |   29.350 ms | 1.00x   |
| **Loam** `WebGpuBackend`         | 249.94 tokens/s |   32.007 ms | 1.09x   |
| onnxruntime WebGPU               |  93.75 tokens/s |   85.329 ms | 2.91x   |
| onnxruntime, its own thread pool |  35.78 tokens/s |  223.560 ms | 7.62x   |
| **Loam** `Cpu`                   |   2.77 tokens/s | 2888.650 ms | 98.42x  |

Loam ran a generated sequence of kernel calls before it ran `compute`. On Wado
2026-10-24 and wasmtime 50.0.0, the rest of the environment as above, that
kernel pass took:

| Implementation                            |      Throughput |     ms/iter |
| ----------------------------------------- | --------------: | ----------: |
| **Loam** `WebGpuBackend`, the kernel pass | 246.02 tokens/s |   32.517 ms |
| **Loam** `Cpu`, the kernel pass           |   6.80 tokens/s | 1177.114 ms |
