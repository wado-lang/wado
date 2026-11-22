# gpt-oss-20b

`model.wado` builds onnx-community's `q4f16` export of gpt-oss-20b and its
tokenizer. The graph holds 55 MB of weights, and seven data files hold the
other 12 GB. A backend keeps every weight but the experts. Its pools hold as
many experts as a budget allows, and a pass fills a slot from the data files
when a token routes to an expert the pools lack.

    package-loam/example/gpt-oss-20b/fetch.sh
    cd package-loam/example/gpt-oss-20b
    wado run cpu.wado -- "The capital of France is"

With no prompt, `cpu.wado` continues each prompt `generate.json` records by 8
tokens. It says whether it picks the first tokens onnxruntime picks, and how far
the logits it picks the first token from are from onnxruntime's:

    mise run loam-oracle-deps
    node generate.mjs
    wado run cpu.wado

`generate.mjs` runs onnxruntime on the CPU alone: the weights outgrow an 8 GB
WebGPU device.

`webgpu.wado` and `pjrt.wado` check the same on a WebGPU device and on a PJRT
plugin's, the budget in bytes their argument gives. Each check says how many of
the experts the tokens picked the pools held already, and writes what each host
step fetched to `build/fetches.json`:

    wado run-webgpu webgpu.wado -- 4500000000
    wado run-pjrt pjrt.wado -- 3000000000

`profile.wado` splits a WebGPU run's time into the experts' reads, the
device's kernels and the rest:

    wado run-webgpu profile.wado -- 4500000000 8

`tools/expert_eviction.wado` replays those fetches under each eviction order and
budget. A run whose budget held some layer's experts whole fetched nothing for
that layer, so record with a budget below every expert:

    wado run ../../tools/expert_eviction.wado -- build/fetches.json 1000000000 3000000000

`analyzed.wado` compiles the model for a `v5e:1x1` through `libtpu.so`, with
every expert held, and prints what the plugin reports of each program
(`scripts/fetch-pjrt-plugin.sh` prints its path):

    TPU_SKIP_MDS_QUERY=1 wado run-pjrt --plugin libtpu.so analyzed.wado

`tokenizer_test.wado` holds the generated tokenizer to Hugging Face's, whose
ids `tokenize.mjs` records in `tokenize.json`.

## What the model asks of Loam

As `wado run tools/onnx_inventory.wado model_q4f16.onnx` reports it. The graph
holds no function and no branch.

| Operator                                       | Nodes |
| ---------------------------------------------- | ----- |
| `QMoE`, 32 experts of 4 bits, 4 to a token     | 24    |
| `GroupQueryAttention`, a window of 128 or none | 24    |
| `SkipSimplifiedLayerNormalization`             | 48    |
| `MatMulNBits`, 4 bits in blocks of 32          | 73    |
| `Add`, `Reshape`, `Cast`, `Constant`, `Gather` | 103   |
| `ReduceSum`, `Shape`, `Sub`                    | 3     |
| `SimplifiedLayerNormalization`                 | 1     |

| Weights                    | Tensors | Bytes          |
| -------------------------- | ------- | -------------- |
| `float16`                  | 317     | 2,459,526,784  |
| `uint8`, packed int4       | 121     | 10,164,326,400 |
| In the graph               | 289     | 54,999,168     |
| In the largest data file   | 62      | 2,096,824,320  |
| One layer's `gate_up_proj` | 1       | 265,420,800    |

The embedding is a `float16` table a `Gather` reads, so the model has no
`GatherBlockQuantized`. The inputs and outputs are `int64` and `float16`, and
the two `Cast`s give `int32`. A weight stays as the checkpoint stores it:
unpacked to bf16, the model's 20 billion weights would outgrow a TPU chip of
16 GB.
