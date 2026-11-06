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

- `fetch_onnxruntime_cuda.sh`: Microsoft's onnxruntime release built for CUDA
  13, and the CUDA libraries its CUDA provider loads (cudart, cuBLAS, cuRAND,
  NVRTC) from NVIDIA's redistributables. A host without an NVIDIA driver skips
  the CUDA arm.
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

Each arm continues "Hello, my name is" by 32 tokens, greedily, after one warmup
run, three times, and prints the tokens it generated, so a divergence between
arms is a bug rather than noise. Each run starts from an empty attention cache:
the first token reads the prompt, and each one after it only itself against
the cache. The throughput counts generated tokens, and `ms/iter` is one run of
32. The programs other than Loam's are the timing `generate.wado` does, on each
runtime's own API, linked against its release as shipped. onnxruntime runs on
the GPU alone: its CUDA arms share `decoder.hpp`, and its WebGPU one,
`onnxruntime_webgpu.mjs`, decodes as the examples' oracle does
(`tools/greedy.mjs`). Each keeps the cache on the device and reads back only
the logits.

## Environment

Wado 2026-11-06 (`-O2`), wasmtime 50.0.1, wgpu-core 29, onnxruntime 1.31.1 on
CUDA 13.2.2 (cuBLAS 13.4.1.3), onnxruntime-node 1.31.1 on Node 26.13.0,
llama.cpp b11846, gcc 15.2.0 (`-O2`), AMD Ryzen 7 5700X (8 cores, 16 threads),
NVIDIA GeForce RTX 4060 Ti (driver 595.99.02, Vulkan), Linux x86_64. `wado`
and `wado-run-webgpu` are release builds, as the tasks build them. Each figure
is the best of three runs.

## GPT-2 (124M)

| Arm                  | Program                                    |
| -------------------- | ------------------------------------------ |
| onnxruntime CUDA     | `gpt2_124m/onnxruntime.cpp`                |
| onnxruntime WebGPU   | `onnxruntime_webgpu.mjs`, Node             |
| Loam `WebGpuBackend` | `gpt2_124m_webgpu.wado`, `wado run-webgpu` |
| Loam `Cpu`           | `gpt2_124m.wado`, `wado run`               |

Every arm generated "John. I'm a writer, and I'm a writer. I'm a writer. I'm a
writer. I'm a writer. I'm a writer".

| Implementation           |      Throughput |     ms/iter | vs best |
| ------------------------ | --------------: | ----------: | ------- |
| onnxruntime CUDA         | 362.11 tokens/s |   88.370 ms | 1.00x   |
| **Loam** `WebGpuBackend` | 195.47 tokens/s |  163.707 ms | 1.85x   |
| onnxruntime WebGPU       |  53.78 tokens/s |  595.003 ms | 6.73x   |
| **Loam** `Cpu`           |   7.44 tokens/s | 4301.615 ms | 48.68x  |

## Gemma 3 (270M)

The Loam and onnxruntime arms read the weights `example/gemma3-270m/fetch.sh`
downloads, in f32. llama.cpp reads GGUF in F16, the precision nearest it.
Google ships no F16 GGUF and ggml-org only Q8_0, so it is unsloth's conversion.

| Arm                         | Program                                             |
| --------------------------- | --------------------------------------------------- |
| onnxruntime CUDA            | `gemma3_270m/onnxruntime.cpp`                       |
| onnxruntime WebGPU          | `onnxruntime_webgpu.mjs`, Node                      |
| llama.cpp Vulkan            | `gemma3_270m/llama_cpp.cpp`                         |
| Loam `WebGpuBackend`        | `gemma3_270m_webgpu.wado`, `wado run-webgpu`        |
| Loam `WebGpuBackend` decode | `gemma3_270m_webgpu.wado decode`, `wado run-webgpu` |
| Loam `Cpu`                  | `gemma3_270m.wado`, `wado run --collector drc`      |

The `WebGpuBackend` arm reads a token back at each step. Its `decode` arm picks
the first token on the host and the rest in one program, with one read back.

Every arm generated "[Your Name] and I am a [Your Profession/Area of Expertise].
I am passionate about [Mention 1-2 key areas of your passion].", the tokens the
example's `generate.json` records.

| Implementation                  |      Throughput |      ms/iter | vs best |
| ------------------------------- | --------------: | -----------: | ------- |
| llama.cpp Vulkan                | 286.19 tokens/s |   111.815 ms | 1.00x   |
| **Loam** `WebGpuBackend` decode | 269.63 tokens/s |   118.683 ms | 1.06x   |
| **Loam** `WebGpuBackend`        | 254.06 tokens/s |   125.954 ms | 1.13x   |
| onnxruntime CUDA                | 142.65 tokens/s |   224.322 ms | 2.01x   |
| onnxruntime WebGPU              | 100.19 tokens/s |   319.407 ms | 2.86x   |
| **Loam** `Cpu`                  |   3.01 tokens/s | 10633.566 ms | 95.10x  |

Loam ran a generated sequence of kernel calls before it ran `compute`. On Wado
2026-10-24 and wasmtime 50.0.0, the rest of the environment as above, that
kernel pass took, over 8 tokens a run:

| Implementation                            |      Throughput |     ms/iter |
| ----------------------------------------- | --------------: | ----------: |
| **Loam** `WebGpuBackend`, the kernel pass | 246.02 tokens/s |   32.517 ms |
| **Loam** `Cpu`, the kernel pass           |   6.80 tokens/s | 1177.114 ms |
