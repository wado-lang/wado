# tiny-gpt2

A GPT-2 decoder with random weights, from
[`fxmarty/onnx-tiny-random-gpt2-without-merge`](https://huggingface.co/fxmarty/onnx-tiny-random-gpt2-without-merge)
on Hugging Face, licensed under the MIT License as its model card states.
`model.onnx` is that repository's `decoder_model.onnx` at commit
`a348808940b73dc20771830a352de90304007387` (SHA-256
`cf43c29cd9a49e0dd88e5c45a921d9a70b87d5a8e6e4099880ebce8d96b887d2`), renamed to
the layout ONNX's backend test data uses.

The repository ships no expected outputs, so `test_data_set_0/` is
onnxruntime's. `oracle.mjs` writes it with onnxruntime-node 1.30.0: four token
ids, a mask of ones, and the logits the model computes from them.
