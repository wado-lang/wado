# tiny-gpt2 with a past

The decoder of [`../`](../README.md) that takes back the keys and values it
computed, from the same repository and commit, under the same MIT License.
`model.onnx` is that repository's `decoder_with_past_model.onnx` (SHA-256
`1b497271e2eeb60061891b9be60a268940ab342991eb3e2ba5f4a39ea11ecbdb`), renamed
as `../model.onnx` is.

It computes one position against a cache of `past_sequence_length` positions,
and returns the cache grown by it. The graph carries its weights, so it runs
from `Weights::embedded()`.

The repository ships no expected outputs, so `test_data_set_0/` and
`test_data_set_1/` are onnxruntime's. `oracle.mjs` writes them with
onnxruntime-node 1.30.0. The prompt runs through `../model.onnx`, which returns
its keys and values, and each data set is one step after it: the token fed and
the logits computed for it against the cache the step before it left.
