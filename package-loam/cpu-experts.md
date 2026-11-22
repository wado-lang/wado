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
- The guest's own memory addresses 4 GiB, and the experts the GPU lacks at
  5 GB take 5.75 GB. A second memory of 64-bit addresses holds them whole,
  which needs `memory64` in Wado.
- The GPU drops to low clocks when idle. Each layer waiting on the CPU idles
  it, which may keep its kernels at the 18 GB/s measured under partial load.

## Design

1. Placement. The GPU pools stay as they are, under the eviction order the
   replay ranks best. The guest holds every other expert in its 64-bit memory.
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

## Keeping the GPU busy

The GPU reaches its full speed only with no gap in its queue: a gap lowers its
clocks, and its kernels then run at the 18 GB/s of partial load. A layer's MoE
output gates the next layer's attention, and the next token gates on the last
layer, so a single sequence offers the GPU no other token's work to fill a
gap. The design has to remove the gaps rather than fill them.

- Balance each layer. The GPU waits for the CPU by the CPU's time less its
  own. The choice per expert minimizes the longer of the two, not each
  expert's own cost: when the CPU's share would outlast the GPU's, the misses
  it would finish last are written into slots instead, and the GPU computes
  them.
- Submit ahead. The host step queues every kernel that does not read the
  CPU's outputs before it waits for them, so the queue holds the held experts,
  the early router of the next layer, and the next layer's writes.
- More tokens a pass. Speculative decoding, as Strata's prompt lookup does,
  checks several drafted tokens in one pass: the GPU reads each weight once
  for all of them, and a greedy run picks the same tokens. The CPU's share
  grows with the experts the extra tokens route to, so this one is measured
  against the balance above.

## Parallelism: the decision

In B, the host runs N instances of a kernel module, one thread each, and each
holds a share of the experts in its own memory.

| Option                 | Cores | Cost                                                       |
| ---------------------- | ----- | ---------------------------------------------------------- |
| A. One guest core      | 1     | none beyond the design                                     |
| B. Worker instances    | N     | a host feature in `wado run-webgpu`, and a Wado API for it |
| C. The PJRT CPU plugin | all   | both hosts in one runner; XLA's speed on 4-bit is unknown  |

The recommendation is A first. It needs no host change, and its measurements
(T, the round trip, the GPU's clocks under the new pattern) decide whether the
1/1.5 target needs B.

## To measure first

- The round trip of one host step on `wado run-webgpu`: a download of the
  picks and the rows, then an upload of the outputs.
- The GPU's time for a token once no expert is written, with the pools as
  they are.
- The hit rate over a longer run than 14 tokens.
- The product's error against onnxruntime with i8 activations.
- The product's speed from a 64-bit memory, whose loads the engine checks
  explicitly rather than by guard pages.
