.PHONY: build hello run test clean

build:
	cargo build

hello: build
	cargo run --quiet -- example/hello.wado

hello-run: hello
	wasmtime run -S p3=y -W component-model-async=y -W component-model-async-stackful=y --invoke 'run()' example/$<.wasm

test:
	cargo test

clean:
	cargo clean
	rm -f example/*.wat
