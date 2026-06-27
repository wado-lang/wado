# Brotli example

Compresses and decompresses bytes using the [`mizchi:brotli`](https://wa.dev/mizchi:brotli)
Wasm component from [wa.dev](https://wa.dev), imported directly by Wado.

## Fetch the component

The component is not committed. Fetch it into `build/` (gitignored) with
[`wkg`](https://github.com/bytecodealliance/wasm-pkg-tools):

```sh
wkg get mizchi:brotli --registry wa.dev -o example/brotli/build/
```

This writes `build/mizchi_brotli@0.2.0.wasm`, exporting two interfaces:

```wit
interface compress {
  compress: func(data: list<u8>) -> list<u8>;
  compress-with-quality: func(data: list<u8>, quality: u32) -> list<u8>;
}
interface decompress {
  decompress: func(data: list<u8>) -> result<list<u8>, string>;
}
```

## Run

```sh
wado run example/brotli/src/main.wado
```

`src/main.wado` imports the `.wasm` component with
`use { Compress } from "../build/...wasm" with { type: "wasm" }`. Each exported
CM interface becomes a Wado `interface`, and its functions are called like WASI
methods (`Compress::compress`, `Decompress::decompress`).
