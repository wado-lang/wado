# GPT-2 (124M)

`gpt2.wado` continues a prompt with GPT-2's 124M-parameter model, whose weights
load from a safetensors checkpoint at run time. `fetch.sh` downloads the model
from Hugging Face (MIT) and splits it with `tools/onnx_split.wado`:

    package-loam/example/gpt2-124m/fetch.sh
    cd package-loam/example/gpt2-124m
    wado run gpt2.wado -- "Hello, my name is"

Git ignores the downloads. `wado test` skips this directory, since it builds
only after `fetch.sh` has run.
