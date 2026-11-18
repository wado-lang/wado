# gpt-oss-20b

`model.wado` builds onnx-community's `q4f16` export of gpt-oss-20b and its
tokenizer. The graph holds 55 MB of weights, and seven data files hold the
other 12 GB. A backend keeps every weight but the experts. Its pools hold as
many experts as a budget allows, and a pass fills a slot from the data files
when a token routes to an expert the pools lack.

    package-loam/example/gpt-oss-20b/fetch.sh
    cd package-loam/example/gpt-oss-20b
    wado run cpu.wado -- "The capital of France is"

With no prompt, `cpu.wado` continues each prompt `generate.json` records. It
says whether it picks the tokens onnxruntime picks, and how far the logits it
picks the first token from are from onnxruntime's:

    mise run loam-oracle-deps
    node generate.mjs
    wado run cpu.wado

`generate.mjs` runs onnxruntime on the CPU alone: the weights outgrow an 8 GB
WebGPU device.

`tokenizer_test.wado` holds the generated tokenizer to Hugging Face's, whose
ids `tokenize.mjs` records in `tokenize.json`.
