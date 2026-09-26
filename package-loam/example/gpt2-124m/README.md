# GPT-2 (124M)

`gpt2.wado` continues a prompt with GPT-2's 124M-parameter model, whose weights
load from a safetensors checkpoint at run time:

    package-loam/example/gpt2-124m/fetch.sh
    cd package-loam/example/gpt2-124m
    wado run gpt2.wado -- "Hello, my name is"

## What the build reads

The model is
[`openai-community/gpt2`](https://huggingface.co/openai-community/gpt2) on
Hugging Face at commit `607a30d783dfa663caf39e06633721c8d4cfcd7e`, licensed
under the MIT License as its model card states. The files here are all derived
from that commit, so the build needs no download, and `wado test` compiles the
example:

- `vocab.json` and `merges.txt` are the tokenizer files, as the repository ships
  them (SHA-256
  `196139668be63f3b5d6574427317ae82f612a97c5d1cdaf36ed2256dbf636783` and
  `1ce1664773c50f3e0cc8842619a93edc4624525b728b188a9e0be33b7726adc5`).
- `gpt2.onnxtext` is `onnx/decoder_model.onnx` (SHA-256
  `e3fc9615868ff8f5e0429b892a0f6ca692784ba6c4ca31c4e9ee8218e7cce34f`) with its
  weights left out, as `tools/onnx_split.wado` writes it.
- `gpt2-header.safetensors` is the header of the checkpoint that tool writes.
  The build checks the graph against it, and only running needs the weights.

`fetch.sh` downloads the model, writes the checkpoint beside them, and rewrites
the graph and the header from it. Git ignores the model and the checkpoint.
