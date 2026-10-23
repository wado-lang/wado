# Loam Design

This file holds the design of what was built after
[WEP: Loam](../docs/wep-2026-09-20-loam.md) was completed. The design before
that is in three documents:

- [WEP: Loam](../docs/wep-2026-09-20-loam.md), the design up to its completion.
- [Loam on WebGPU](./webgpu.md), the design of its WebGPU backend.
- [Loam Tokenizer](./tokenizer.md), the design of its tokenizer.

A roadmap item in [TODO.md](./TODO.md) writes its design here when it is
finished. Where it changes a decision the WEP made, it names the WEP's section.

## The Probe

A build names a model's whole checkpoint and reads its header alone, as The
Graph Is the Schema decided. Loam's generator exports `probe`. A `safetensors`
input's extent runs through the end of its header, whose length its first eight
bytes give, so the probe reads those eight. Every other input, a graph or a
tokenizer, is schema throughout, and its extent is the whole file.

The probe is half of it. A Kiln host used to read every input whole and cut it
to the extent afterwards. It now serves the probe's stream a range at a time,
then reads each input to its extent and no further, for `generate`, the cache
key and the cache check alike. `wado test`'s result cache and the run's source
watch record such an input as the prefix it is. The protocol is in
[the Kiln spec](../docs/spec-kiln.md#the-probe).

What a build reads of each checkpoint, counted with
`strace -f -y -e trace=read,pread64` over `wado check [--no-cache] <model.wado>`
in `example/`, summing the reads of the `.safetensors` file:

| Checkpoint     | File size     | Build that runs the generator | Build that hits the cache |
| -------------- | ------------- | ----------------------------- | ------------------------- |
| GPT-2 (124M)   | 652,163,888   | 15,160                        | 15,152                    |
| Gemma 3 (270M) | 1,139,529,588 | 28,016                        | 28,008                    |

A build that runs the generator reads the eight bytes the probe asks for, then
the header.

### The examples

`example/gpt2-124m/` and `example/gemma3-270m/` name the checkpoint `fetch.sh`
writes, and no longer carry a header file of their own. The checkpoint is too
large to commit, so a file naming the model compiles only once `fetch.sh` has
run, and `wado test` leaves those files out. GPT-2's tokenizer test takes its
`Tokenizer` from `tokenizer.json` directly, so it still runs without the
download.

`hf2loam.mjs` read its layout from the header file. It now reads the
initializers `gpt2.onnxtext` declares without values, in the order the graph
lists them, which is the order `tools/onnx_split.wado` writes the checkpoint
in. Its output is byte for byte the checkpoint `fetch.sh` writes.
