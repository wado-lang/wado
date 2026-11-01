# Gemma 3 (270M)

`gemma3.wado` continues a prompt with Gemma 3's 270M-parameter
instruction-tuned model, whose weights load from the model's data file at run
time:

    package-loam/example/gemma3-270m/fetch.sh
    cd package-loam/example/gemma3-270m
    wado run --collector drc gemma3.wado -- "<start_of_turn>user
    Why is the sky blue?<end_of_turn>
    <start_of_turn>model
    "

With no prompt it continues each prompt `generate.json` records, and says
whether it picks the tokens onnxruntime picks and how long a token takes.

The weights take more than half the heap, which is all the default copying
collector can use, so it runs under `--collector drc`.

`gemma3.wado` is the command line around `model.wado`, which holds the tokenizer
and the model as a library. The model keeps the attention cache between
calls, so a call that extends the last one's ids runs only the new ones.

## On a WebGPU device

`webgpu.wado` and `webgpu_compute.wado` continue each prompt `generate.json`
records on `WebGpuBackend`. They print what `gemma3.wado` prints with no prompt,
and the dispatches and read backs a token takes. `webgpu.wado` runs a token as a
`pass` through the kernels, and `webgpu_compute.wado` as a `compute` through
the program the backend compiled for the shape class:

    cd package-loam/example/gemma3-270m
    wado run-webgpu webgpu.wado
    wado run-webgpu webgpu_compute.wado

`continued.wado` holds what the two share.

## What the build reads

The model is
[`onnx-community/gemma-3-270m-it-ONNX`](https://huggingface.co/onnx-community/gemma-3-270m-it-ONNX)
on Hugging Face at commit `2dbbfdb1b59bd034eb959428c6a7da9dd7ea27f0`, an export
of Google's `google/gemma-3-270m-it` under the
[Gemma Terms of Use](https://ai.google.dev/gemma/terms). The graph uses
onnxruntime's `com.microsoft` operators: `GroupQueryAttention` for attention,
and the `Gelu` and `SimplifiedLayerNormalization` functions.

`model.wado` imports `tokenizer.json` and `onnx/model.onnx` by their URLs,
pinned by their SHA-256. The build downloads them once into Wado's cache. The
graph names its weights in `onnx/model.onnx_data`, at an offset and a length
each, and the build reads none of that file. `Prepared::load` reads the weights
from it at run time. A program reaches no file the build read, so `fetch.sh`
downloads the data file into `build/`, which Git ignores, with the model and
the tokenizer files the oracle scripts read.

`wado test` skips this directory, since building the model downloads it. Name
a test file to run it once `fetch.sh` has.

- `tokenize.json` is the oracle for the tokenizer: the ids Hugging Face's
  tokenizer gives each of a set of texts. `tokenize.mjs` writes it with
  `@huggingface/tokenizers`, reading `build/tokenizer.json` and
  `build/tokenizer_config.json`. `tokenizer_test.wado` checks Loam's against
  it.
- `generate.json` is the oracle for the model: the tokens onnxruntime picks
  greedily after each prompt, and its time per token on the CPU and on WebGPU,
  which must pick the same tokens. `generate.mjs` writes it, running
  `build/model.onnx` with `onnxruntime-node` and feeding each step the attention
  cache, as `model.wado` does. `generate_test.wado` checks
  Loam's tokens against it:

      wado test --collector drc generate_test.wado
