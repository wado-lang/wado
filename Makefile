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
		echo ""; \
		echo "Development environment ready."; \
	fi

.PHONY: build
build: wado-compiler/lib/builtins/wado-bundled.wat
	cargo build

.PHONY: hello
hello:
	cargo run -p wado-cli --quiet -- compile -O2 -o example/hello.wat example/hello.wado
	cargo run -p wado-cli --quiet -- compile -O2 -o example/hello.wasm example/hello.wado

.PHONY: hello-run
hello-run:
	cargo run -p wado-cli --quiet -- run example/hello.wado

.PHONY: hello-run-wasmtime
hello-run-wasmtime: hello
	wasmtime run -S p3=y -W component-model-async=y -W component-model-async-stackful=y --invoke 'run()' example/hello.wasm

.PHONY: test
test:
	cargo test

.PHONY: test-cov
test-cov:
	cargo llvm-cov --all-features --workspace

.PHONY: test-cov-html
test-cov-html:
	cargo llvm-cov --all-features --workspace --html
	@echo "Coverage report generated at target/llvm-cov/html/index.html"

.PHONY: on-task-done
on-task-done: clippy-fix update-bundled update-golden-fixtures test format
	@echo "All artifacts are up-to-date and tested."

.PHONY: format
format:
	cargo fmt --verbose --all
	dprint fmt

.PHONY: format-wado
format-wado:
	cargo run --bin wado --quiet -- format -w $$(grep -L '"compile_error"' wado-compiler/tests/fixtures/*.wado wado-compiler/tests/fixtures/**/*.wado)

.PHONY: update-golden-fixtures
update-golden-fixtures:
	@echo "Updating golden fixtures..."
	@mkdir -p wado-compiler/tests/fixtures.golden
	@cargo run --bin wado --quiet -- dump --optimize --unparse -O2 \
		-o 'wado-compiler/tests/fixtures.golden/{name}.lowered.wado' \
		wado-compiler/tests/fixtures/*.wado
	@echo "Golden fixtures updated."

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
	rm -f benchmark/*.wasm benchmark/count_prime_c benchmark/mandelbrot_c benchmark/sieve_c
	$(MAKE) -C wasm-size clean

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
	cargo run --bin wado --quiet -- syntax --format tmLanguage -o wado-vscode/syntaxes/wado.tmLanguage.json
	cargo run --bin wado --quiet -- syntax --format language-config -o wado-vscode/language-configuration.json
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
benchmark-count-prime:
	@echo "=== Compiling Wado benchmark ==="
	cargo run --bin wado --quiet -- compile -O2 -o benchmark/count_prime.wasm benchmark/count_prime.wado
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
	@echo "=== Python ==="
	@python3 benchmark/count_prime.py
	@echo ""
	@echo "=== Ruby ==="
	@ruby benchmark/count_prime.rb
	@echo ""
	@echo "=== Wado (wasmtime) ==="
	@wasmtime run -S p3=y -W gc=y -W function-references=y -W component-model-async=y -W component-model-async-stackful=y --invoke 'run()' benchmark/count_prime.wasm

.PHONY: benchmark-mandelbrot
benchmark-mandelbrot:
	@echo "=== Compiling Wado benchmark ==="
	cargo run --bin wado --quiet -- compile -O2 -o benchmark/mandelbrot.wasm benchmark/mandelbrot.wado
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
	@echo "=== Python ==="
	@python3 benchmark/mandelbrot.py
	@echo ""
	@echo "=== Ruby ==="
	@ruby benchmark/mandelbrot.rb
	@echo ""
	@echo "=== Wado (wasmtime) ==="
	@wasmtime run -S p3=y -W gc=y -W function-references=y -W component-model-async=y -W component-model-async-stackful=y --invoke 'run()' benchmark/mandelbrot.wasm

.PHONY: report-wasm-size
report-wasm-size:
	$(MAKE) -C wasm-size report-wasm-size

.PHONY: benchmark-sieve
benchmark-sieve:
	@echo "=== Compiling Wado benchmark ==="
	cargo run --bin wado --quiet -- compile -O2 -o benchmark/sieve.wasm benchmark/sieve.wado
	@echo ""
	@echo "=== Compiling C benchmark ==="
	cc -O3 -o benchmark/sieve_c benchmark/sieve.c
	@echo ""
	@echo "=== C (cc -O3) ==="
	@./benchmark/sieve_c
	@echo ""
	@echo "=== JavaScript (Node.js) ==="
	@node benchmark/sieve.js
	@echo ""
	@echo "=== Python ==="
	@python3 benchmark/sieve.py
	@echo ""
	@echo "=== Ruby ==="
	@ruby benchmark/sieve.rb
	@echo ""
	@echo "=== Wado (wasmtime) ==="
	@wasmtime run -S p3=y -W gc=y -W function-references=y -W component-model-async=y -W component-model-async-stackful=y --invoke 'run()' benchmark/sieve.wasm
