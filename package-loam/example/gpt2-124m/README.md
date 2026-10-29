# GPT-2 (124M)

`gpt2.wado` continues a prompt with GPT-2's 124M-parameter model, whose weights
load from the model file at run time:

    package-loam/example/gpt2-124m/fetch.sh
    cd package-loam/example/gpt2-124m
    wado run gpt2.wado -- "Hello, my name is"

`gpt2.wado` is the command line around `model.wado`, which holds the tokenizer
and the model as a library. A browser page can build the same library into a
component.

`webgpu.wado` runs the same model on a WebGPU device, and checks that it picks
the tokens onnxruntime picks:

    wado run-webgpu webgpu.wado

## What the build reads

The model is
[`openai-community/gpt2`](https://huggingface.co/openai-community/gpt2) on
Hugging Face at commit `607a30d783dfa663caf39e06633721c8d4cfcd7e`, licensed
under the MIT License as its model card states. The files here are all derived
from that commit:

- `tokenizer.json` is the tokenizer, as the repository ships it (SHA-256
  `8414cab924d8b9b33013f0d221c5862f365ee9be39c5c2bfae8a5a9e970478a6`). Loam
  generates `Tokenizer` from it.
- `tokenize.json` is the oracle for the tokenizer: the ids Hugging Face's
  tokenizer gives each of a set of texts. `tokenize.mjs` writes it with
  `@huggingface/tokenizers` 0.2.0, reading `tokenizer.json` and the repository's
  `tokenizer_config.json`.
- `generate.json` is the oracle for the model: the tokens onnxruntime picks
  greedily after a prompt, and its time per token on the CPU. `generate.mjs`
  writes it, running `onnx/decoder_model.onnx` with `onnxruntime-node`.

`model.wado` imports `onnx/decoder_model.onnx` by its URL, pinned by its SHA-256.
The build downloads it once into Wado's cache. Its weights are in the file, so
`embed: false` leaves them there: the build reads past them, and
`Prepared::load` reads them from the same file at run time. A program reaches no
file the build read, so `fetch.sh` downloads the model as `model.onnx`, which
Git ignores.

`wado test` leaves out the files naming the model, since building one downloads
it. `tokenizer_test.wado` needs no download and runs. `generate_test.wado`
checks Loam's tokens against `generate.json`, once `fetch.sh` has run:

    wado test generate_test.wado
