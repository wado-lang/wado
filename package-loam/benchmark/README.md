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
- `gpt_oss_20b/fetch_gguf.sh`: gpt-oss-20b as ggml-org's MXFP4 GGUF, for
  llama.cpp.

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
llama.cpp b12464, gcc 15.2.0 (`-O2`), AMD Ryzen 7 5700X (8 cores, 16 threads),
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
| onnxruntime CUDA         | 380.94 tokens/s |   84.003 ms | 1.00x   |
| **Loam** `WebGpuBackend` | 311.19 tokens/s |  102.832 ms | 1.22x   |
| onnxruntime WebGPU       |  52.07 tokens/s |  614.575 ms | 7.32x   |
| **Loam** `Cpu`           |   7.55 tokens/s | 4237.188 ms | 50.44x  |

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

Every arm but llama.cpp generated "[Your Name] and I am a [Your
Profession/Area of Expertise]. I am passionate about [Mention 1-2 key areas of
your passion].", the tokens the example's `generate.json` records. llama.cpp
picks other tokens from the 19th on.

| Implementation                  |      Throughput |      ms/iter | vs best |
| ------------------------------- | --------------: | -----------: | ------- |
| **Loam** `WebGpuBackend` decode | 295.50 tokens/s |   108.291 ms | 1.00x   |
| llama.cpp Vulkan                | 264.95 tokens/s |   120.778 ms | 1.12x   |
| **Loam** `WebGpuBackend`        | 259.84 tokens/s |   123.151 ms | 1.14x   |
| onnxruntime CUDA                | 155.15 tokens/s |   206.249 ms | 1.90x   |
| onnxruntime WebGPU              | 107.12 tokens/s |   298.743 ms | 2.76x   |
| **Loam** `Cpu`                  |   3.04 tokens/s | 10526.001 ms | 97.20x  |

### The `q4` and `q4f16` exports

`mise run gemma3-270m-quantized` runs the same prompt on the `q4` and `q4f16`
exports, whose matrices hold 4-bit weights, on WebGPU alone. llama.cpp runs
GGUF quantized by its own `Q4_0` and `Q4_1` schemes, which hold other weights
than the exports, so its rows are for reference:

| Arm                         | Program                                                |
| --------------------------- | ------------------------------------------------------ |
| onnxruntime WebGPU          | `onnxruntime_webgpu.mjs`, `decoderQ4` / `decoderQ4f16` |
| llama.cpp Vulkan            | `gemma3_270m/llama_cpp.cpp`, `Q4_0` / `Q4_1`           |
| Loam `WebGpuBackend` decode | `gemma3_270m/q4_webgpu.wado`, `q4f16_webgpu.wado`      |

Loam picks the tokens `generate_q4.json` and `generate_q4f16.json` record.
onnxruntime's WebGPU provider picks other tokens from one run to the next on
both exports.

| Implementation                          |      Throughput |    ms/iter | vs best |
| --------------------------------------- | --------------: | ---------: | ------- |
| **Loam** `WebGpuBackend` decode `q4`    | 487.80 tokens/s |  65.600 ms | 1.00x   |
| llama.cpp Vulkan `Q4_0`                 | 363.49 tokens/s |  88.035 ms | 1.34x   |
| llama.cpp Vulkan `Q4_1`                 | 360.62 tokens/s |  88.735 ms | 1.35x   |
| **Loam** `WebGpuBackend` decode `q4f16` | 338.58 tokens/s |  94.511 ms | 1.44x   |
| onnxruntime WebGPU `q4`                 | 174.58 tokens/s | 183.299 ms | 2.79x   |
| onnxruntime WebGPU `q4f16`              | 139.89 tokens/s | 228.754 ms | 3.49x   |

## gpt-oss-20b

The model's 12 GB outgrow the RTX 4060 Ti's 8, and onnxruntime holds every
weight on the device, so llama.cpp is the arm to reach. It runs the experts of
the layers `--n-cpu-moe` names on the CPU, where they stay, and the rest on the
device; with none on the CPU, the device runs out of memory. Its release reads
ggml-org's MXFP4 GGUF, the precision OpenAI released the experts in:

```sh
cd package-loam/benchmark
gpt_oss_20b/llama_bench.sh 10 14 24
```

| Layers' experts on the CPU | Prompt of 13   | 32 tokens after it |
| -------------------------- | -------------- | ------------------ |
| 10                         | 75.89 tokens/s | 42.40 tokens/s     |
| 14                         | 80.25 tokens/s | 35.96 tokens/s     |
| 24                         | 54.92 tokens/s | 25.14 tokens/s     |

Loam's `WebGpuBackend` holds 5 GB of the experts on the device and writes the
rest there as a token picks them. `example/gpt-oss-20b/profile.wado` times the
prompt of 5 and 8 tokens after it: 1.60 tokens/s over the whole, 26 times as
long a token as llama.cpp's 10 layers on the CPU.
