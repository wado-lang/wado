# tiny-gpt2

A GPT-2 decoder with random weights, from
[`fxmarty/onnx-tiny-random-gpt2-without-merge`](https://huggingface.co/fxmarty/onnx-tiny-random-gpt2-without-merge)
on Hugging Face, licensed under the MIT License as its model card states.
`model.onnx` is that repository's `decoder_model.onnx` at commit
`a348808940b73dc20771830a352de90304007387` (SHA-256
`cf43c29cd9a49e0dd88e5c45a921d9a70b87d5a8e6e4099880ebce8d96b887d2`), renamed to
the layout ONNX's backend test data uses.

`vocab.json` and `merges.txt` are the tokenizer files from that commit, as the
repository ships them (SHA-256
`2c2bb27afe24f304c7883ed6529abd4f092e53882701eaa22d444c90f0f5a784` and
`06e116ab37805f782ce4493bf28984f565d2c9019d14a1b932d0a67f97d85147`).

`model.onnxtext` and `model.safetensors` are `model.onnx` split by Loam's own
tool, the graph with its weights left out and the checkpoint holding them:

    cd package-loam/tests/models/tiny-gpt2
    wado run ../../../tools/onnx_split.wado -- model.onnx model

The repository ships no expected outputs, so `test_data_set_0/` is
onnxruntime's. `oracle.mjs` writes it with onnxruntime-node 1.30.0: four token
ids, a mask of ones, and the logits the model computes from them.

`generate.json` is the oracle for text. `generate.mjs` writes it with
onnxruntime-node 1.30.0 and `@huggingface/tokenizers` 0.2.0, reading the
repository's `tokenizer.json` and `tokenizer_config.json`. It records the ids
the tokenizer gives each of a set of texts, and the tokens onnxruntime picks
greedily after each of a set of prompts.
