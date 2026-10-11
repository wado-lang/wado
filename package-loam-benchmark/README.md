# Loam Benchmarks

Loam's backends against onnxruntime, in the format of
[`benchmark/`](../benchmark/README.md): each arm prints a throughput line, so
`benchmark/pick.ts` and `benchmark/ab.ts` read the logs.

## Setup

The models are not in the repository. Fetch them and install onnxruntime:

```sh
package-loam/example/gpt2-124m/fetch.sh
mise run loam-oracle-deps
```

## Tasks

```sh
cd package-loam-benchmark
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
| Loam `WebGpuBackend`             | `gpt2_124m_webgpu.wado`, `wado run-webgpu` |
| Loam `Cpu`                       | `gpt2_124m.wado`, `wado run`               |

`generate.wado` is the timing both Loam arms share. Every arm recomputes the
whole sequence at each token, since the graph takes no key-value cache.
