# MNIST-12

A handwritten digit classifier from the [ONNX Model Zoo](https://github.com/onnx/models),
licensed under the MIT License as its
[model card](https://github.com/onnx/models/blob/main/validated/vision/classification/mnist/README.md)
states. Unpacked from
`validated/vision/classification/mnist/model/mnist-12.tar.gz` at commit
`4c46cd00fbdb7cd30b6c1c17ab54f2e1f4f7b177` (SHA-256
`a53a59dcaca8804a0f6dfda9a3cf2e082979589391dbde73640b60684f1d24e9`).

`model.onnx` is the archive's `mnist-12.onnx`, renamed to the layout ONNX's
backend test data uses, so `test_data_set_0/` sits beside it as it does there.
Its `output_0.pb` is the oracle Loam's output is compared against.
