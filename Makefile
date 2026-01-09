.PHONY: build hello hello-run hello-run-wasmtime test clean fmt clippy

build:
	cargo build

hello: build
	cargo run --quiet -- compile example/hello.wado

hello-run: build
	cargo run --quiet -- run example/hello.wado

hello-run-wasmtime: hello
	wasmtime run -S p3=y -W component-model-async=y -W component-model-async-stackful=y --invoke 'run()' example/hello.wasm

test:
	cargo test

fmt:
	cargo fmt

clippy:
	cargo clippy

clean:
	cargo clean
	rm -f example/*.wat example/*.wasm
