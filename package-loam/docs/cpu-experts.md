# Experts on the CPU

A design sketch for roadmap item 1 of [TODO.md](../TODO.md): gpt-oss-20b at
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

| What                                                                  | Figure                                     |
| --------------------------------------------------------------------- | ------------------------------------------ |
| One guest core, one product of i8 activations alone, DRAM             | 10.84 GB/s of stored bytes, 1.29 ms/expert |
| One guest core, `cpu_experts.wado`'s whole expert, 15-bit activations | 4.68 GB/s, 3.0 ms/expert                   |
| Host time of a write to a slot, one layer's `gate_up_proj` at once    | 3.96 GB/s                                  |
| A slot's copy out of host memory, the write's first step              | 16.5 GB/s                                  |
| GPU pools of 5 GB, `LayerOrder`: a step's experts held                | 80.8% of 1344, 258 MB loaded a token       |
| The same, a step filling at most 2 slots a layer                      | 80.4%, 0.8% on the host, 253 MB a token    |
| The same, a step filling at most 1                                    | 80.0%, 5.2% on the host, 199 MB a token    |
| The same, a step filling none                                         | 69.0%, 31.0% on the host                   |
| GPU pools of 5 GB, `Farthest` (the bound)                             | 87.1%                                      |
| GPU pools of 6.3 GB, as llama.cpp's best arm, `Recent`                | 84.7%                                      |

```sh
(cd package-loam && wado run -O2 --no-spectre-mitigation tools/cpu_expert_speed.wado)
(cd package-loam && wado run tools/expert_eviction.wado -- \
    example/gpt-oss-20b/build/fetches.json 5000000000 6300000000)
(cd package-loam/example/gpt-oss-20b && \
    wado run-webgpu -O2 --no-spectre-mitigation --dir . profile.wado -- 5000000000 0)
```

The replay covers 14 generated tokens. The first row timed one product's i8
dot alone, without its zero point; i8 activations part from the
decomposition by 0.9% on `experts_case`, past the 1e-4 a device's product is
held to, so the kernel quantizes them to 15 bits. It reads the scales as the
export stores them, f16, and widens them as it goes, as the host holds every
expert once.

## Measured on gpt-oss-20b

The benchmark's 32 tokens, with the GPU's pools at 5 GB:

| Experts the pools lack                                                | Throughput     |
| --------------------------------------------------------------------- | -------------- |
| Written into a slot from the files                                    | 1.59 tokens/s  |
| Computed on one guest core, as `cpu_experts.wado` does                | 5.69 tokens/s  |
| Written from host memory, at most 2 a layer a step, the rest computed | 8.87 tokens/s  |
| Not written, nor computed (the bound the pass sets)                   | 24.28 tokens/s |

```sh
(cd package-loam/benchmark && mise run gpt-oss-20b)
```

The last row left the call to `written` in `HeldExperts::fetched` out by hand.
In the second, the decode steps write nothing, so the pools hold what the
prompt left in them, and the host computes 39% of the picks. In the third, the
host computes 0.7% of them, and the writes take most of a token's time: the
32 tokens and their prompt write 9.0 GB.

## The budget

The target, 1/1.5 of llama.cpp's 40.38 tokens/s, is 37.1 ms a token. At 5 GB
the GPU lacks 19.2% of 96 picks, 18.4 experts, and one core computes them in
24 ms. The GPU's own work overlaps that, so a token takes the longer of the
two plus one round trip per layer. One core fits only if the GPU's part and 24
round trips together take under about 13 ms.

## Constraints

- The guest has one thread. A Component Model thread is cooperative, so one
  instance computes on one core, while llama.cpp and Strata use all of them.
- The guest's own memory addresses 4 GiB, and the experts the GPU lacks at
  5 GB take 5.75 GB. A `LinearArray64` holds them whole. Its accesses are
  bounds-checked one by one, and the figure above is with
  `--no-spectre-mitigation`, as every benchmark runs.
- The GPU drops to low clocks when idle. Each layer waiting on the CPU idles
  it, which may keep its kernels at the 18 GB/s measured under partial load.

## Design

1. Placement. The GPU pools stay as they are, under the eviction order the
   replay ranks best. `WebGpuBackend` holds every expert of a shared pool's
   layers in a `LinearArray64`, which `HeldExperts::pools` hands it at load,
   and writes a slot from there rather than from the files.
2. Choice per expert. A write costs the host about what computing the expert
   does, and the expert then stays for the tokens after. A step of one token
   writes the first 2 misses of a layer into slots and computes the rest on
   the CPU; a longer step writes every miss.
3. One layer. The host step answers slot -1 for an expert the pools lack.
   WebGPU's `Graph::expert_mixture` reads the layer's input rows and the
   picks' weights back, queues the held experts' products, computes the rest
   on the host while the device runs them, and adds them.
4. The kernel. It is Wado on `core:simd`, in `cpu_experts.wado`; `Cpu` stays
   the reference and keeps its speed. Activations are quantized to 15 bits per
   block of 32, each split into two i8 halves that `relaxed_dot` multiplies
   exactly, and four rows share each block's activations.
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

## To measure

- The round trip of one host step on `wado run-webgpu`: a download of the
  picks and the rows, then an upload of the outputs.
- How much of a write's host time each of its three copies takes. A write
  from host memory copies the bytes three times: out of the `LinearArray64`,
  across the component boundary into the host's own buffer, and into wgpu's
  staging. Over the 32 timed tokens, the host's thread was busy 80% of the
  wall time, and `memmove` took 47.5% of that:

  ```sh
  perf record -k CLOCK_MONOTONIC -g -F 999 -- wado run-webgpu -O2 \
      --no-spectre-mitigation --dir . profile.wado -- 5000000000 32
  ```
