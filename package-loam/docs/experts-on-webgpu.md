# The Experts on WebGPU

A design sketch for roadmap item 1 of [TODO.md](../TODO.md): gpt-oss-20b at
1/1.5 of llama.cpp's speed on the RTX 4060 Ti. It makes a pass cheap on the
host. Holding the experts the device lacks is a separate part of the item.

## What a token costs

A step of gpt-oss-20b with every expert write left out, so that only the pass
remains, on the RTX 4060 Ti machine:

| What                                         | Figure                          |
| -------------------------------------------- | ------------------------------- |
| A token                                      | 157 ms, 6.39 tokens/s           |
| The host thread busy                         | 75% of it, about 117 ms a token |
| The device's kernels, each timed alone       | about 33 ms a pass              |
| Dispatches                                   | about 2,600 a pass, 108 a layer |
| Of a layer's, the experts' and their routing | about 90                        |
| Weights copied out of the pools              | about 1.2 GB a token            |

The host's time splits into Loam's own code (30 ms a token), wgpu (28 ms), the
driver (16 ms), the Component Model's lifts and lowers of the `wasi:webgpu`
calls (14 ms), and allocation, the kernel and the collector (29 ms). Each
dispatch costs the host about 45 us, so the dispatch count sets the time.
Submitting every 64 dispatches rather than 16 changed nothing.

```sh
(cd package-loam/example/gpt-oss-20b && wado run-webgpu --no-spectre-mitigation --dir . profile.wado -- 5000000000 8)
```

The figures above left the expert writes out of `HeldExperts::fetched` by hand,
and timed the host with `perf record` over `--profile=perfmap`.

`mixture_of_experts` runs each pick as a `repeated` loop over operations. A
decode step's expert, per pick:

| Dispatches | What                                                          |
| ---------: | ------------------------------------------------------------- |
|          5 | `Gather`s copying the expert's weights and scales out of pool |
|          2 | the two `QuantizedProduct`s                                   |
|        ~12 | the bias, the SwiGLU, the weight and the sum, in small groups |

The routing adds about 15 more a layer, `arg_max` and its masks four times.

## The plan

In order, each measured before the next:

1. The experts' composite. The device runs a layer's experts in two dispatches,
   reading each expert where its pool holds it.
2. A gather read in place. A kernel reads a row its operand gathers from where
   the table holds it, rather than from a copy, on every model.
3. A fixed count of host syncs a token. What the host waits for does not grow
   with the layers.

### The experts' composite

[A composite is a computation no fusion reaches](./devices.md#a-composite-is-a-computation-no-fusion-reaches)
already counts the mixture of experts among the composites. The host step
splits `QMoE` in two: the routing, which the step reads, and the experts, which
read what the step gives back. So the composite is the second half,
`Graph::expert_mixture`: given each token's row, its picks and their weights,
the slots the step answers, and the two projections' pools, the weighted sum of
the picks' outputs. Its decomposition is what `by_picks` and `by_experts`
compute now, so its meaning is Loam's own, as the sampler's is, and the
routing stays operations before it.

The recorder records a decode step's form, fewer picks than experts, as one
node, `Operation::ExpertMixture`, unless it decomposes. A prompt's form is
recorded as its operations, which fuse: run as one node, its decomposition ran
each operation alone, and a prompt of 13 tokens ran the device out of memory.
`Cpu` and `PjrtBackend` run the decomposition. `WebGpuBackend` runs the node
as two quantized products a pick, each reading the expert's weight, scales and
bias where its pool holds them:

- `gated_up` is a paired product over the token's row, the weight's rows
  alternating between gate and up. Its epilogue adds the bias and takes the
  SwiGLU of each pair, so it writes the pick's inner row.
- `weighed_down` runs the inner row through the second projection. Its epilogue
  adds the bias, weighs the result by the pick's weight, zero where that is not
  positive, and adds the running total of the token's earlier picks.

The sum runs in pick order, as the decomposition's loop does, so the products
differ from it only by what
[Results Are Held to a Tolerance, Not to the Bit](../../docs/wep-2026-09-20-loam.md#results-are-held-to-a-tolerance-not-to-the-bit)
allows. A pick whose expert the pools lack, and that its step does not write
into a slot, runs on the host, as [Experts on the CPU](./cpu-experts.md)
describes.

A layer's experts then take 2 dispatches a pick, 8 for gpt-oss-20b's 4, and
nothing is copied out of a pool. The same step with the expert writes left out:

| Experts                       | Throughput     | A token |
| ----------------------------- | -------------- | ------- |
| Decomposed                    | 6.39 tokens/s  | 157 ms  |
| Two products a pick, in place | 24.28 tokens/s | 41.2 ms |

```sh
(cd package-loam/benchmark && mise run gpt-oss-20b)
```

Both rows left the call to `written` in `HeldExperts::fetched` out by hand.
With the experts the device lacks written in from the files, the step ran at
1.59 tokens/s; [Experts on the CPU](./cpu-experts.md#measured-on-gpt-oss-20b)
measures it with them written from host memory or computed on the host.

### A gather read in place

A `gather` whose result one kernel reads is a view of its table, as a slice
is: the kernel reads each row at the index the gather reads it by. That holds
for a quantized product's weight, an embedding's rows, and a cache's positions
alike, so the composite's kernels are one case of it rather than its own rule.
It is designed once the composite is measured.

### A fixed count of host syncs a token

A layer's host step reads its picks back, so a token waits on the device once a
layer. Whether it must is decided with the CPU's experts, since those read the
picks on the host anyway.
