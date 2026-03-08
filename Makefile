.PHONY: on-task-started
on-task-started:
	@echo "Setting up development environment..."
	@if ! command -v mise >/dev/null 2>&1; then \
		echo "Installing mise..."; \
		curl -fsSL https://mise.run | sh; \
		echo ""; \
		echo "mise installed. Please restart your shell or run:"; \
		echo "  echo 'eval \"\$$(~/.local/bin/mise activate bash)\"' >> ~/.bashrc  # for bash"; \
		echo "  echo 'eval \"\$$(~/.local/bin/mise activate zsh)\"' >> ~/.zshrc   # for zsh"; \
		echo ""; \
		echo "Then run 'make on-task-started' again to install project tools."; \
	else \
		echo "mise is already installed."; \
		echo "Installing project tools..."; \
		mise install; \
		echo "Installing optional tools..."; \
		mise install github:cli/cli@latest 2>&1 || echo "  [skip] github:cli/cli (install failed, non-fatal)"; \
		echo ""; \
		echo "Development environment ready."; \
	fi

RUSTFLAGS ?= -D warnings

.PHONY: build
build: wado-compiler/lib/builtins/wado-bundled-libm.wat
	RUSTFLAGS="$(RUSTFLAGS)" cargo check
	RUSTFLAGS="$(RUSTFLAGS)" cargo check -p wado-compiler --target wasm32-unknown-unknown
	RUSTFLAGS="$(RUSTFLAGS)" cargo check -p wado-manifest --target wasm32-unknown-unknown

.PHONY: hello
hello:
	RUSTFLAGS="$(RUSTFLAGS)" cargo run -p wado-cli -- compile -O2 -o example/hello.wat  example/hello.wado
	RUSTFLAGS="$(RUSTFLAGS)" cargo run -p wado-cli -- compile -O2 -o example/hello.wasm example/hello.wado

.PHONY: hello-run
hello-run:
	RUSTFLAGS="$(RUSTFLAGS)" cargo run -p wado-cli -- run example/hello.wado

.PHONY: hello-run-wasmtime
hello-run-wasmtime: hello
	wasmtime run -S p3=y -W component-model=y -W component-model-gc=y -W component-model-async=y -W component-model-async-builtins=y -W component-model-async-stackful=y -W gc=y -W function-references=y -W simd=y -W wide-arithmetic=y -W threads=y --invoke 'run()' example/hello.wasm

.PHONY: test
test:
	RUSTFLAGS="$(RUSTFLAGS)" RUST_TEST_THREADS=12 cargo test

.PHONY: test-wado
test-wado:
	cargo run -p wado-cli -- test example/*.wado wado-compiler/lib/core benchmark/*/*.wado wasm-size/*/*.wado

.PHONY: test-cov
test-cov:
	cargo llvm-cov --all-features --workspace

.PHONY: test-cov-html
test-cov-html:
	cargo llvm-cov --all-features --workspace --html
	@echo "Coverage report generated at target/llvm-cov/html/index.html"

.PHONY: on-task-done
on-task-done: clippy-fix update-golden-fixtures update-golden-format-fixtures doc-stdlib format test test-wado
	@echo "All artifacts are up-to-date and tested."

.PHONY: doc-stdlib
doc-stdlib:
	mise run doc-stdlib

.PHONY: format
format:
	cargo fmt --verbose --all
	dprint fmt

.PHONY: format-wado
format-wado:
	cargo run --bin wado -- format -w $$(grep -L '"compile_error"' wado-compiler/tests/fixtures/**/*.wado)

.PHONY: update-golden-fixtures
update-golden-fixtures:
	@mkdir -p wado-compiler/tests/fixtures.golden
	@rm -rf wado-compiler/tests/fixtures.golden/*.*
	@cargo run --bin wado -- dump --wir -O2 \
		-o 'wado-compiler/tests/fixtures.golden/{name}.wir.wado' \
		wado-compiler/tests/fixtures/*.wado

.PHONY: update-golden-format-fixtures
update-golden-format-fixtures:
	mise run update-golden-format-fixtures


.PHONY: clippy
clippy:
	RUSTFLAGS="$(RUSTFLAGS)" cargo clippy --all --all-features -- -D warnings

.PHONY: clippy-fix
clippy-fix:
	cargo clippy --all --all-features --fix --allow-dirty --allow-staged

.PHONY: clean
clean:
	cargo clean
	rm -f example/*.wat example/*.wasm
	mise run -C benchmark clean
	mise run -C wasm-size clean

# VS Code extension targets
.PHONY: install-wado-vscode-dev
install-wado-vscode-dev:
	@if [ -e ~/.vscode/extensions/wado-lang.wado-0.0.1 ]; then \
		echo "wado-vscode is already installed"; \
	else \
		cd wado-vscode && npm install && npm run compile; \
		ln -s "$(CURDIR)/wado-vscode" ~/.vscode/extensions/wado-lang.wado-0.0.1; \
		echo "wado-vscode installed. Restart VS Code to activate."; \
	fi

.PHONY: clean-wado-vscode-dev
clean-wado-vscode-dev:
	rm -f ~/.vscode/extensions/wado-lang.wado-0.0.1
	@echo "wado-vscode symlink removed. Restart VS Code to deactivate."

.PHONY: test-wado-vscode
test-wado-vscode:
	cd wado-vscode && npm install && npm run test:unit && npm run test

.PHONY: update-wado-vscode-grammar
update-wado-vscode-grammar:
	cargo run --bin wado -- syntax --format tmLanguage -o wado-vscode/syntaxes/wado.tmLanguage.json
	cargo run --bin wado -- syntax --format language-config -o wado-vscode/language-configuration.json
	@echo "Updated wado-vscode syntax files"

.PHONY: update-json-schema-files
update-json-schema-files:
	@mkdir -p wado-cli/schemas
	@echo "Downloading TextMate grammar schema..."
	@curl -sL "https://raw.githubusercontent.com/martinring/tmlanguage/master/tmlanguage.json" \
		| sed 's|"$$schema": "http://json-schema.org/schema#"|"$$schema": "http://json-schema.org/draft-07/schema#"|g; s|"id":|"$$id":|g' \
		> wado-cli/schemas/tmlanguage.schema.json
	@echo "Downloading VS Code language-configuration schema..."
	@curl -sL "https://raw.githubusercontent.com/SchemaStore/schemastore/master/src/schemas/json/language-configuration.json" \
		> wado-cli/schemas/language-configuration.schema.json
	@echo "Updated JSON schema files in wado-cli/schemas/"

.PHONY: update-vendor
update-vendor:
	git submodule update --remote vendor/wasm vendor/wasi vendor/wasmtime vendor/wasm-tools

.PHONY: update-stdlib-wasi
update-stdlib-wasi:
	rm -f wado-compiler/lib/wasi/*.wado
	cargo run -p wado-from-wit -- \
		--wit-dir vendor/wasmtime/crates/wasi-http/src/p3/wit \
		--output-dir wado-compiler/lib/wasi
	cargo run -p wado-from-wit -- \
		--wit-dir vendor/wasmtime/crates/wasi/src/p3/wit \
		--output-dir wado-compiler/lib/wasi
	rm -rf wado-compiler/lib/wasi/wasi-http.wado # a file for bindgen

wado-compiler/lib/builtins/wado-bundled-libm.wat: Cargo.toml Cargo.lock wado-bundled-libm/Cargo.toml wado-bundled-libm/src/lib.rs
	make update-bundled

.PHONY: update-bundled
update-bundled:
	cd wado-bundled-libm && CARGO_PROFILE_RELEASE_OPT_LEVEL=s CARGO_PROFILE_RELEASE_LTO=true cargo build --release
	wasm-tools print target/wasm32-unknown-unknown/release/wado_bundled_libm.wasm > wado-compiler/lib/builtins/wado-bundled-libm.wat

.PHONY: benchmark-all
benchmark-all:
	mise run -C benchmark all

.PHONY: benchmark-count-prime
benchmark-count-prime:
	mise run -C benchmark count-prime

.PHONY: benchmark-mandelbrot
benchmark-mandelbrot:
	mise run -C benchmark mandelbrot

.PHONY: benchmark-sieve
benchmark-sieve:
	mise run -C benchmark sieve

.PHONY: benchmark-zlib
benchmark-zlib:
	mise run -C benchmark zlib

.PHONY: benchmark-fts
benchmark-fts:
	mise run -C benchmark fts

.PHONY: benchmark-json-twitter
benchmark-json-twitter:
	mise run -C benchmark json-twitter

.PHONY: benchmark-json-canada
benchmark-json-canada:
	mise run -C benchmark json-canada

.PHONY: benchmark-json-catalog
benchmark-json-catalog:
	mise run -C benchmark json-catalog

.PHONY: report-wasm-size
report-wasm-size:
	mise run -C wasm-size report-wasm-size
