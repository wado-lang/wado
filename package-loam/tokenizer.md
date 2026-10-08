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

Today each GPT-2 example builds its tokenizer from `vocab.json` and
`merges.txt` through code beside the examples, which knows only GPT-2's
pipeline.

## Decision

### `tokenizer.json` is the source

A program imports a tokenizer as it imports a graph, through Loam's generator:

```wado
use { Tokenizer } from "./tokenizer.json" with {
    generator: { module: "lib:loam" },
};

let ids = Tokenizer::encode(&text);
let back = Tokenizer::decode(&ids);
```

The generator tells a tokenizer from a graph by the `.json` extension. The
tokenizer is imported apart from the model, since an ONNX graph does not carry
one.

### The pipeline is decided at build time

The generator reads the pipeline and emits code for the stages it names and no
others. A stage, a model kind or a pattern construct it does not support stops
the build and names itself, as an operator does in a graph. Nothing is left to
interpret at run time:

- A `pre_tokenizer` pattern compiles to a matcher, so the run needs no regular
  expression engine. Unicode classes such as `\p{L}` come from `core:icu`.
- The vocabulary and the merges are embedded as data, as `Weights::embedded()`
  embeds a graph's weights.

The kernels the stages share live in Loam's runtime library beside the tensor
kernels, and the generated module imports them as it imports those. So the ids
a tokenizer gives are the type a model's `forward` takes.

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

- [ ] GPT-2's pipeline. Finished when both GPT-2 examples encode and decode
  through a `Tokenizer` generated from their `tokenizer.json`, with the ids
  Hugging Face's tokenizer gives, and `example/gpt2_tokenizer.wado` is gone.
