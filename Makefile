.PHONY: build
build:
	cargo build

.PHONY: hello
hello: build
	cargo run -p wado-cli --quiet -- compile -o example/hello.wat example/hello.wado
	cargo run -p wado-cli --quiet -- compile -o example/hello.wasm example/hello.wado

.PHONY: hello-run
hello-run: build
	cargo run -p wado-cli --quiet -- run example/hello.wado

.PHONY: hello-run-wasmtime
hello-run-wasmtime: hello
	wasmtime run -S p3=y -W component-model-async=y -W component-model-async-stackful=y --invoke 'run()' example/hello.wasm

.PHONEY: hello-validate
hello-validate: hello
	wasm-tools validate --verbose --features=cm-async,cm-async-stackful,cm-async-builtins,gc example/hello.wat
	wasm-tools validate --verbose --features=cm-async,cm-async-stackful,cm-async-builtins,gc example/hello.wasm


.PHONY: test
test:
	cargo test

.PHONY: format
format:
	cargo fmt
	npx prettier --write spec.md AGENTS.md README.md docs/*.md

.PHONY: clippy
clippy:
	cargo clippy

.PHONY: clippy-fix
clippy-fix:
	cargo clippy --fix --allow-dirty --allow-staged

.PHONY: clean
clean:
	cargo clean
	rm -f example/*.wat example/*.wasm

.PHONY: update-vendor
update-vendor:
	git submodule update --remote vendor/wasm vendor/wasi vendor/wasmtime vendor/wasm-tools

.PHONY: update-stdlib-wasi
update-stdlib-wasi: build
	rm -f wado-compiler/lib/wasi/*.wado
	cargo run -p wado-from-wit -- \
		--wit-dir vendor/wasmtime/crates/wasi/src/p3/wit \
		--output-dir wado-compiler/lib/wasi

.PHONY: update-bundled
update-bundled:
	cd wado-bundled && cargo build --release
	wasm-tools print target/wasm32-unknown-unknown/release/wado_bundled.wasm > wado-compiler/lib/builtins/wado-bundled.wat

.PHONY: check-bundled
check-bundled:
	@echo "Checking if wado-bundled.wat is up-to-date..."
	@cd wado-bundled && cargo build --release --quiet
	@wasm-tools print target/wasm32-unknown-unknown/release/wado_bundled.wasm > /tmp/wado-bundled-check.wat
	@diff -q wado-compiler/lib/builtins/wado-bundled.wat /tmp/wado-bundled-check.wat || \
		(echo "ERROR: wado-bundled.wat is out of date. Run 'make update-bundled' to regenerate." && exit 1)
	@echo "wado-bundled.wat is up-to-date."
