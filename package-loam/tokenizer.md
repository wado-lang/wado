# Loam Tokenizer

Text reaches a model as token ids, and a tokenizer turns one into the other.
This is the design of the tokenizer Loam ships, step 10 of the roadmap in
[WEP: Loam](../docs/wep-2026-09-20-loam.md).

## Context

A tokenizer is not a model. It is a fixed string algorithm over data that
statistics over a corpus produced: a vocabulary, and for BPE the order of its
merges. Running it computes nothing learned.

Models differ less in the algorithm than in the stages around it. Hugging
Face's `tokenizer.json` writes the whole pipeline down as data:

1. `normalizer`: rewrite the text, such as NFKC or lowercasing.
2. `pre_tokenizer`: split the text into pieces, often by a regular expression.
3. `model`: turn each piece into ids, by BPE, WordPiece or Unigram.
4. `post_processor`: add special tokens around the ids.
5. `decoder`: turn ids back into text.

`added_tokens` lists the special tokens, which are matched in the text before
any of these stages run.

## Decision

### `tokenizer.json` is the source

A program imports a tokenizer as it imports a graph, through Loam's generator:

```wado
use { Tokenizer } from "./tokenizer.json" with {
    type: "hf-tokenizer",
    generator: { module: "lib:loam" },
};

let ids = Tokenizer::encode(&text);
let back = Tokenizer::decode(&ids);
```

The use site's `type` tells the generator what the file is, never the
extension: `"hf-tokenizer"` here, for Hugging Face's `tokenizer.json` format,
and `"onnx"` or `"onnxtext"` for a graph. The tokenizer is
imported apart from the model, since an ONNX graph does not carry one.

### The pipeline is decided at build time

The generator reads the pipeline and emits code for the stages it names and no
others. A stage, a model kind or a pattern construct it does not support stops
the build and names itself, as an operator does in a graph. Nothing is left to
interpret at run time:

- A `pre_tokenizer` pattern is a matcher, so the run needs no regular
  expression engine. `ByteLevel` fixes GPT-2's pattern, so its matcher is
  written once in the runtime. Unicode classes such as `\p{L}` come from
  `core:icu`.
- The vocabulary and the merges are embedded as data, one entry to a line in a
  byte literal, as `Weights::embedded()` embeds a graph's weights.

The stages run in Loam's runtime library beside the tensor kernels
(`BpeTokenizer` in `src/runtime/tokenizer.wado`), and the generated module
imports them as it imports those.

### Support grows by the models that need it

The first pipeline supported is GPT-2's: the `ByteLevel` pre-tokenizer, BPE, and
the `ByteLevel` decoder. Another model's pipeline adds the stages it names,
each checked against the ids Hugging Face's `tokenizers` gives for that model.

### Hugging Face's tokenizer is an oracle

As with onnxruntime, Hugging Face's `tokenizers` is run for expected ids and
never read. The format of `tokenizer.json` is learned from the files
themselves.

### Encoding only, no training

Loam builds a tokenizer from a `tokenizer.json` and never trains one. Training
belongs where the model is trained.

## Roadmap

- [x] GPT-2's pipeline. Both GPT-2 examples encode and decode through a
  `Tokenizer` generated from their `tokenizer.json`, with the ids Hugging Face's
  tokenizer gives: `conformance/tiny_gpt2_tokenizer_test.wado` against the tiny
  GPT-2's `generate.json`, and `example/gpt2-124m/tokenizer_test.wado` against
  `tokenize.json`. `src/tokenizer_json.wado` refuses every other stage and
  setting by name.
