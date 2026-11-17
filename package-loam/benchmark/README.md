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
- `fetch_llama_cpp.sh`: llama.cpp's Vulkan release, the one `llama_cpp.lock`
  pins. `mise run update-llama-cpp` pins the newest.
- `gemma3_270m/fetch_gguf.sh`: Gemma 3 (270M) as GGUF, for llama.cpp: F16,
  and Q4_0 and Q4_1 for the `q4` table.

The WebGPU arm of onnxruntime runs onnxruntime-node, which
`mise run loam-oracle-deps` installs as `package-lock.json` pins it. Without
it the arm is skipped.

## Tasks

```sh
cd package-loam/benchmark
mise run gpt2-124m     # GPT-2 (124M) greedy generation
mise run gemma3-270m   # Gemma 3 (270M) greedy generation
mise run gemma3-270m-quantized   # its q4 and q4f16 exports
mise run all
```

Each arm continues "Hello, my name is" by 32 tokens, greedily, after one warmup
run, three times, and prints the tokens it generated, so a divergence between
arms is a bug rather than noise. Each run starts from an empty attention cache:
the first token reads the prompt, and each one after it only itself against
the cache. The throughput counts generated tokens, and `ms/iter` is one run of
32. The programs other than Loam's are the timing `generate.wado` does, on each
runtime's own API, linked against its release as shipped. onnxruntime runs on
the GPU alone: its CUDA arms share `decoder.hpp`, and its WebGPU one is
`onnxruntime_webgpu.mjs`. Each keeps the cache on the device and reads back
only the next token, which `tools/picking.mjs` appends to the graph, as Loam's
decode arms pick theirs on the device.

### Tuning onnxruntime

Each step's pick in the graph is what moves onnxruntime: on Gemma 3 it runs
1.15 times as fast on WebGPU's `q4` and 1.17 times on CUDA as when reading back
the logits. On WebGPU, none of these moved its `q4` decode past run-to-run
noise:

- `validationMode: 'disabled'`
- `freeDimensionOverrides` for `batch_size`
- `storageBufferCacheMode: 'simple'`
- `executionMode: 'parallel'`
- `enableGraphCapture`
- feeding the picked token back as a device tensor, which ran 1.12 times as
  long: the graph's shape operators read `input_ids` on the host

## Device time per kernel

`gemma3_270m/profile.wado` prints how much device time each kernel of Gemma
3's decode took, by `WebGpuBackend::profile`'s timestamps. Without an argument
it runs the f32 weights; with `q4`, the `q4` export.

```sh
cd package-loam/benchmark
wado run-webgpu --dir ../example/gemma3-270m gemma3_270m/profile.wado q4
```

## Environment

Wado 2026-11-17 (`-O2`), wasmtime 50.0.1, wgpu-core 29, onnxruntime 1.31.1 on
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
| onnxruntime CUDA         | 380.56 tokens/s |   84.087 ms | 1.00x   |
| **Loam** `WebGpuBackend` | 311.91 tokens/s |  102.592 ms | 1.22x   |
| onnxruntime WebGPU       |  51.60 tokens/s |  620.107 ms | 7.37x   |
| **Loam** `Cpu`           |   7.58 tokens/s | 4219.954 ms | 50.19x  |

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
| llama.cpp Vulkan                | 286.11 tokens/s |   111.844 ms | 1.00x   |
| **Loam** `WebGpuBackend` decode | 276.19 tokens/s |   115.862 ms | 1.04x   |
| **Loam** `WebGpuBackend`        | 222.36 tokens/s |   143.913 ms | 1.29x   |
| onnxruntime CUDA                | 161.72 tokens/s |   197.877 ms | 1.77x   |
| onnxruntime WebGPU              | 106.65 tokens/s |   300.059 ms | 2.68x   |
| **Loam** `Cpu`                  |   2.95 tokens/s | 10839.958 ms | 96.92x  |

### The `q4` and `q4f16` exports

`mise run gemma3-270m-quantized` runs the same prompt on the `q4` and `q4f16`
exports, whose matrices hold 4-bit weights, on WebGPU alone:

| Arm                         | Program                                                |
| --------------------------- | ------------------------------------------------------ |
| onnxruntime WebGPU          | `onnxruntime_webgpu.mjs`, `decoderQ4` / `decoderQ4f16` |
| Loam `WebGpuBackend` decode | `gemma3_270m/q4_webgpu.wado`, `q4f16_webgpu.wado`      |

Loam picks the tokens `generate_q4.json` and `generate_q4f16.json` record.
onnxruntime's WebGPU provider picks other tokens from one run to the next on
both exports.

| Implementation                          |      Throughput |    ms/iter | vs best |
| --------------------------------------- | --------------: | ---------: | ------- |
| **Loam** `WebGpuBackend` decode `q4`    | 398.50 tokens/s |  80.301 ms | 1.00x   |
| **Loam** `WebGpuBackend` decode `q4f16` | 310.41 tokens/s | 103.088 ms | 1.28x   |
| onnxruntime WebGPU `q4`                 | 172.83 tokens/s | 185.152 ms | 2.31x   |
| onnxruntime WebGPU `q4f16`              | 138.81 tokens/s | 230.538 ms | 2.87x   |
