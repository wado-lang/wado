# tiny-gpt2, merged

The decoder of [`../`](../README.md) exported as one graph. It branches with an
`If` on `use_cache_branch`: one branch reads the prompt alone, and the other
reads it against the keys and values computed before. Both branches read one
set of weights, held once at the top of the graph.

`model.onnx` is `decoder_model_merged.onnx` from
[`fxmarty/onnx-tiny-random-gpt2-with-merge`](https://huggingface.co/fxmarty/onnx-tiny-random-gpt2-with-merge)
on Hugging Face at commit `bc3de2f77e05b49d622b4e963ed068c61c4674f2` (SHA-256
`a0ec0acc8897b2c98fca29549ff71ded54b86d4e9152d0791db6c093dd10f99f`), licensed
under the MIT License as its model card states. It is renamed as `../model.onnx`
is. The graph carries its weights, so it runs from `Prepared::embedded`.

The repository ships no expected outputs, so the `test_data_set_<n>/`
directories are onnxruntime's. `oracle.mjs` writes them with onnxruntime-node
1.30.0. The first is the prompt, run without a past against an empty cache.
Each after it is one step against the cache the run before it left. Each holds
the tokens fed and the logits computed for them.

The logits are bit for bit those of `../test_data_set_0/` and
`../with-past/test_data_set_<n>/`, so the weights are the separate decoders'.
`../generate.json` holds for this model too.
