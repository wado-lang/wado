# Loam — ONNX Models as Wado Modules

`wado-lang:loam` is a Kiln generator that compiles an ONNX graph into a Wado
module ahead of time, and the runtime that module runs on. Each tensor's axes
are named from the graph and checked at build time, so a shape mismatch stops
the build rather than a run. A module runs on the CPU, on a WebGPU device, or
on a PJRT plugin such as XLA's.
[`docs/design.md`](./docs/design.md) holds the design, and
[`TODO.md`](./TODO.md) the roadmap and the known gaps.

## Using Loam

A consumer names Loam twice: as the generator, and as the runtime the
generated module imports.

```toml
[build-dependencies]
"wado-lang:loam" = { version = "^0.0.39" }

[dependencies]
"wado-lang:loam" = { version = "^0.0.39" }
```

A `use` names the model and what to import from it. `type` is `onnx` for a
binary model and `onnxtext` for ONNX's text form, and `generator.options`
fixes what the graph leaves open, such as a batch size:

```wado
use { Cpu, Tensor } from "wado-lang:loam";
use { Model, N as Batch, X1 as In, Y1 as Out, Weights, compute } from "./mlp.onnxtext"
    with {
        type: "onnxtext",
        generator: { module: "wado-lang:loam" },
    };

fn infer(weights: Weights, x: &Tensor<f32, [Batch, In]>) -> Tensor<f32, [Batch, Out]> {
    let mut cpu = Cpu {};
    let mut model = Model::new(weights.params(&mut cpu));
    return compute(&mut cpu, x, &mut model);
}
```

The import renames the axes it reads, as any import renames, and a tensor's
axes are part of its type: passing the output where the input is expected is a
compile error.

`hf-tokenizer` imports a Hugging Face `tokenizer.json` as a `Tokenizer`, so a
program reads text in and writes text out without a tokenizer of its own.

## Examples

`example/` holds programs that run models, from the two-layer MLP above to
GPT-2, Gemma 3 and gpt-oss. Each says how to run it, and a large model's
directory says how to fetch its weights.

```sh
wado run package-loam/example/mlp.wado
wado run package-loam/example/gpt2.wado -- "Hello, my name is"
```

## Tools

`loam gen` prints the module the generator writes, and `loam dump` the checked
graph; [`docs/tools.md`](./docs/tools.md) says how to use them and the
inventory and inspection scripts. [`benchmark/`](./benchmark/README.md) times
Loam's backends against onnxruntime.
