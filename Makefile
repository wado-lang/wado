.PHONY: build
build:
	cargo build

.PHONY: hello
hello: build
	cargo run --quiet -- compile -o example/hello.wat example/hello.wado

.PHONY: hello-run
hello-run: build
	cargo run --quiet -- run example/hello.wado

.PHONY: hello-run-wasmtime
hello-run-wasmtime: hello
	wasmtime run -S p3=y -W component-model-async=y -W component-model-async-stackful=y --invoke 'run()' example/hello.wasm

.PHONY: test
test:
	cargo test

.PHONY: format
format:
	cargo fmt
	npx prettier --write spec.md compiler.md AGENTS.md

.PHONY: clippy
clippy:
	cargo clippy

.PHONY: clean
clean:
	cargo clean
	rm -f example/*.wat example/*.wasm

.PHONY: update-vendor
update-vendor:
	git submodule update --remote vendor/wasm vendor/wasi vendor/wasmtime vendor/wasm-tools
