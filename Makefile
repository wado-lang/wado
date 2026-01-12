.PHONY: build
build: wado-compiler/lib/builtins/wado-bundled.wat
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

.PHONY: on-task-done
on-task-done: format clippy-fix update-bundled test
	@echo "All artifacts are up-to-date and tested."

.PHONY: format
format:
	cargo fmt --verbose --all
	npx prettier --write spec.md AGENTS.md README.md docs/*.md

.PHONY: clippy
clippy:
	cargo clippy --all --all-features

.PHONY: clippy-fix
clippy-fix:
	cargo clippy --all --all-features --fix --allow-dirty --allow-staged

.PHONY: clean
clean:
	cargo clean
	rm -f example/*.wat example/*.wasm
	rm -f benchmark/*.wasm benchmark/count_prime_c benchmark/mandelbrot_c

.PHONY: update-vendor
update-vendor:
	git submodule update --remote vendor/wasm vendor/wasi vendor/wasmtime vendor/wasm-tools

.PHONY: update-stdlib-wasi
update-stdlib-wasi: build
	rm -f wado-compiler/lib/wasi/*.wado
	cargo run -p wado-from-wit -- \
		--wit-dir vendor/wasmtime/crates/wasi/src/p3/wit \
		--output-dir wado-compiler/lib/wasi

wado-compiler/lib/builtins/wado-bundled.wat: Cargo.toml Cargo.lock wado-bundled/Cargo.toml wado-bundled/src/lib.rs
	make update-bundled

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

.PHONY: benchmark-count-prime
benchmark-count-prime: build
	@echo "=== Compiling Wado benchmark ==="
	cargo run --bin wado --quiet -- compile -o benchmark/count_prime.wasm benchmark/count_prime.wado
	@echo ""
	@echo "=== Compiling C benchmark ==="
	cc -O3 -o benchmark/count_prime_c benchmark/count_prime.c
	@echo ""
	@echo "=== C (cc -O3) ==="
	@./benchmark/count_prime_c
	@echo ""
	@echo "=== JavaScript (Node.js) ==="
	@node benchmark/count_prime.js
	@echo ""
	@echo "=== Wado (wasmtime) ==="
	@time wasmtime run -S p3=y -W gc=y -W function-references=y -W component-model-async=y -W component-model-async-stackful=y --invoke 'run()' benchmark/count_prime.wasm

.PHONY: benchmark-mandelbrot
benchmark-mandelbrot: build
	@echo "=== Compiling Wado benchmark ==="
	cargo run --bin wado --quiet -- compile -o benchmark/mandelbrot.wasm benchmark/mandelbrot.wado
	@echo ""
	@echo "=== Compiling C benchmark ==="
	cc -O3 -ffp-contract=off -o benchmark/mandelbrot_c benchmark/mandelbrot.c
	@echo ""
	@echo "=== C (cc -O3) ==="
	@./benchmark/mandelbrot_c
	@echo ""
	@echo "=== JavaScript (Node.js) ==="
	@node benchmark/mandelbrot.js
	@echo ""
	@echo "=== Wado (wasmtime) ==="
	@time wasmtime run -S p3=y -W gc=y -W function-references=y -W component-model-async=y -W component-model-async-stackful=y --invoke 'run()' benchmark/mandelbrot.wasm
