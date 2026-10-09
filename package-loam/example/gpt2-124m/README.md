# GPT-2 (124M)

`gpt2.wado` continues a prompt with GPT-2's 124M-parameter model, whose weights
load from a safetensors checkpoint at run time:

    package-loam/example/gpt2-124m/fetch.sh
    cd package-loam/example/gpt2-124m
    wado run gpt2.wado -- "Hello, my name is"

`gpt2.wado` is the command line around `model.wado`, which holds the tokenizer
and the model as a library. A browser page can build the same library into a
component.

`hf2loam.mjs` writes the checkpoint from Hugging Face's `model.safetensors`
without running Wado. Its `convert` function takes and returns byte arrays, so a
browser page can run it too.

## What the build reads

The model is
[`openai-community/gpt2`](https://huggingface.co/openai-community/gpt2) on
Hugging Face at commit `607a30d783dfa663caf39e06633721c8d4cfcd7e`, licensed
under the MIT License as its model card states. The files here are all derived
from that commit, so the build needs no download, and `wado test` compiles the
example:

- `tokenizer.json` is the tokenizer, as the repository ships it (SHA-256
  `8414cab924d8b9b33013f0d221c5862f365ee9be39c5c2bfae8a5a9e970478a6`). Loam
  generates `Tokenizer` from it.
- `tokenize.json` is the oracle for the tokenizer: the ids Hugging Face's
  tokenizer gives each of a set of texts. `tokenize.mjs` writes it with
  `@huggingface/tokenizers` 0.2.0, reading `tokenizer.json` and the repository's
  `tokenizer_config.json`.
- `gpt2.onnxtext` is `onnx/decoder_model.onnx` (SHA-256
  `e3fc9615868ff8f5e0429b892a0f6ca692784ba6c4ca31c4e9ee8218e7cce34f`) with its
  weights left out, as `tools/onnx_split.wado` writes it.
- `gpt2-header.safetensors` is the header of the checkpoint that tool writes.
  The build checks the graph against it, and only running needs the weights.

`fetch.sh` downloads the model, writes the checkpoint beside them, and rewrites
the graph and the header from it. Git ignores the model and the checkpoint.
