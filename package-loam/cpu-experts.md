# Experts on the CPU

A design sketch for roadmap item 1 of [TODO.md](./TODO.md): gpt-oss-20b at
1/1.5 of llama.cpp's speed on the RTX 4060 Ti. The GPU holds as many experts as
its budget allows. Today an expert it lacks is written into a pool slot before
the layer runs. Under this design, the CPU computes that expert where the bytes
already are, and only the expert's input and output cross PCIe.

## What others do

The sources are documents and papers, never code.

- llama.cpp's `--n-cpu-moe N`
  ([guide](https://huggingface.co/blog/Doctor-Shotgun/llamacpp-moe-offload-guide))
  keeps the experts of the first N layers in RAM, and the CPU computes them.
  Attention, the router and the dense weights stay on the GPU. For a prompt of
  32 tokens or more, it copies the CPU's weights to the GPU instead. The
  placement is static, by layer.
- Fiddler ([arXiv 2402.07033](https://arxiv.org/abs/2402.07033)) places the
  most popular experts on the GPU. For each expert a pass routes s tokens to,
  it computes on the GPU if the GPU holds the expert. Otherwise it copies the
  weights over if `cpu(s) > gpu(s) + copy`, and else it computes on the CPU.
  GPU time is about flat in s, and CPU time grows with s. Moving the
  activations costs under 1%.
- Strata ([how it works](https://github.com/Niko1221/Strata/blob/main/docs/HOW_IT_WORKS.md))
  holds attention, the routers and a cache of the most-used experts in VRAM,
  and keeps learning which experts those are. RAM holds every expert. The CPU
  computes the uncached ones in place, with AVX-512 or AVX2, while the GPU
  computes the cached ones. During attention it prefetches the next layer's
  experts over PCIe. Its other gains are speculative decoding (MTP heads,
  prompt lookup), and neither changes the tokens a greedy run picks.

## Measured here

On the RTX 4060 Ti machine, a Ryzen 7 5700X with 8 cores, 32 MiB of L3 and no
AVX-512. One expert is 14,014,080 bytes as the `q4f16` export stores it.

| What                                                     | Figure                                     |
| -------------------------------------------------------- | ------------------------------------------ |
| One guest core, 4-bit weights times i8 activations, DRAM | 11.18 GB/s of stored bytes, 1.25 ms/expert |
| GPU pools of 5 GB, `LayerOrder`: a step's experts held   | 80.8% of 1344, 258 MB loaded a token       |
| GPU pools of 5 GB, `Farthest` (the bound)                | 87.0%                                      |
| GPU pools of 6.3 GB, as llama.cpp's best arm, `Recent`   | 84.7%                                      |

```sh
(cd package-loam/benchmark/gpt_oss_20b && wado run -O3 cpu_expert.wado)
(cd package-loam && wado run tools/expert_eviction.wado -- \
    example/gpt-oss-20b/build/fetches.json 5000000000 6300000000)
```

The replay covers 14 generated tokens. A longer run may hold fewer.

## The budget

The target, 1/1.5 of llama.cpp's 40.38 tokens/s, is 37.1 ms a token. At 5 GB
the GPU lacks 19.2% of 96 picks, 18.4 experts, and one core computes them in
23 ms. The GPU's own work overlaps that, so a token takes the longer of the
two plus one round trip per layer. One core fits only if the GPU's part and 24
round trips together take under about 14 ms.

## Constraints

- The guest has one thread. A Component Model thread is cooperative, so one
  instance computes on one core, while llama.cpp and Strata use all of them.
- The guest addresses 4 GiB, and Wado has no `memory64`. The experts the GPU
  lacks at 5 GB take 5.75 GB, so some stay in the file, which the page cache
  serves when a token picks them, at the cost of a copy before the product.
- The GPU drops to low clocks when idle. Each layer waiting on the CPU idles
  it, which may keep its kernels at the 18 GB/s measured under partial load.

## Design

1. Placement. The GPU pools stay as they are, under the eviction order the
   replay ranks best. The guest holds as many of the remaining experts as its
   heap allows, the most-picked first, and reads the rest from the file.
2. Choice per expert, after Fiddler. In a host step, an expert the pools lack
   is computed on the CPU when the step routes fewer than T tokens to it, and
   written into a slot otherwise, as today. A generated token routes 1. T is
   measured: the token count where the CPU's time passes a write's.
3. One layer. The host step already downloads the router's picks. It also
   downloads the layer's input rows, 2880 f32s for each token. The GPU runs
   the held experts while the CPU runs the rest, and the host uploads the
   CPU's outputs, which the routing weights then sum with the GPU's.
4. The kernel. It is Wado on `core:simd`, part of `WebGpuBackend`'s host step;
   `Cpu` stays the reference and keeps its speed. Activations are quantized to
   i8 per block of 32, as the measured kernel does. That parts from the GPU's
   f16 product, and the tolerance check against onnxruntime decides whether it
   holds. If it does not, the kernel computes in f32, at a speed to measure.
5. Ahead of time. The TODO item Experts loaded ahead applies the next layer's
   router early. It then also tells the CPU which guest-held experts to warm
   and which file ranges to read.

## Parallelism: the decision

In B, the host runs N instances of a kernel module, one thread each, and each
holds a share of the experts in its own memory.

| Option                 | Cores | 4 GiB             | Cost                                                       |
| ---------------------- | ----- | ----------------- | ---------------------------------------------------------- |
| A. One guest core      | 1     | stays             | none beyond the design                                     |
| B. Worker instances    | N     | split N ways      | a host feature in `wado run-webgpu`, and a Wado API for it |
| C. The PJRT CPU plugin | all   | the plugin's heap | both hosts in one runner; XLA's speed on 4-bit is unknown  |

The recommendation is A first. It needs no host change, and its measurements
(T, the round trip, the GPU's clocks under the new pattern) decide whether the
1/1.5 target needs B. B is the general answer, since it lifts both limits at
once.

## To measure first

- The round trip of one host step on `wado run-webgpu`: a download of the
  picks and the rows, then an upload of the outputs.
- The GPU's time for a token once no expert is written, with the pools as
  they are.
- The hit rate over a longer run than 14 tokens.
- The product's error against onnxruntime with i8 activations.
