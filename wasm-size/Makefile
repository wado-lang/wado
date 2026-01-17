# Wasm Size Comparison
# Compares .wasm binary sizes across different languages

WASMTIME := wasmtime

# C compiler for wasm (homebrew: llvm + lld + wasi-libc + wasi-runtimes)
LLVM_PATH ?= /opt/homebrew/opt/llvm
LLD_PATH ?= /opt/homebrew/opt/lld
WASI_LIBC_PATH ?= /opt/homebrew/opt/wasi-libc
WASI_RUNTIMES_PATH ?= /opt/homebrew/opt/wasi-runtimes
WASI_SYSROOT := $(WASI_LIBC_PATH)/share/wasi-sysroot
WASM_CLANG := PATH=$(LLD_PATH)/bin:$$PATH $(LLVM_PATH)/bin/clang --target=wasm32-wasi --sysroot=$(WASI_SYSROOT) -resource-dir $(WASI_RUNTIMES_PATH)/share/wasi-runtimes

# Output directories
OUT_DIR := out

# Programs to benchmark
PROGRAMS := hello_world pi_approx

.PHONY: all clean report-wasm-size install-deps
.PHONY: build-c build-rust build-zig build-tinygo build-assemblyscript build-moonbit build-wado
.PHONY: validate

all: report-wasm-size

$(OUT_DIR):
	mkdir -p $(OUT_DIR)

# === C (homebrew: llvm + lld + wasi-libc + wasi-runtimes) ===
build-c: $(OUT_DIR)
	@if [ -x "$(LLVM_PATH)/bin/clang" ] && [ -x "$(LLD_PATH)/bin/wasm-ld" ] && [ -d "$(WASI_SYSROOT)" ] && [ -d "$(WASI_RUNTIMES_PATH)/share/wasi-runtimes" ]; then \
		echo "Building C..."; \
		$(WASM_CLANG) -Oz -Wl,--strip-all -o $(OUT_DIR)/hello_world_c.wasm c/hello_world.c; \
		$(WASM_CLANG) -Oz -Wl,--strip-all -o $(OUT_DIR)/pi_approx_c.wasm c/pi_approx.c; \
	else \
		echo "SKIP: C (brew install llvm lld wasi-libc wasi-runtimes)"; \
	fi

# === Rust ===
build-rust: $(OUT_DIR)
	@echo "Building Rust..."
	cd rust/hello_world && cargo build --release --target wasm32-wasip1
	cd rust/pi_approx && cargo build --release --target wasm32-wasip1
	cp rust/hello_world/target/wasm32-wasip1/release/hello_world.wasm $(OUT_DIR)/hello_world_rust.wasm
	cp rust/pi_approx/target/wasm32-wasip1/release/pi_approx.wasm $(OUT_DIR)/pi_approx_rust.wasm

# === Zig ===
build-zig: $(OUT_DIR)
	@if command -v zig >/dev/null 2>&1; then \
		echo "Building Zig..."; \
		zig build-exe -O ReleaseSmall -target wasm32-wasi zig/hello_world.zig -femit-bin=$(OUT_DIR)/hello_world_zig.wasm; \
		zig build-exe -O ReleaseSmall -target wasm32-wasi zig/pi_approx.zig -femit-bin=$(OUT_DIR)/pi_approx_zig.wasm; \
	else \
		echo "SKIP: Zig (brew install zig)"; \
	fi

# === TinyGo ===
build-tinygo: $(OUT_DIR)
	@echo "Building TinyGo..."
	tinygo build -o $(OUT_DIR)/hello_world_tinygo.wasm -target=wasip1 -opt=z -no-debug tinygo/hello_world.go
	tinygo build -o $(OUT_DIR)/pi_approx_tinygo.wasm -target=wasip1 -opt=z -no-debug tinygo/pi_approx.go

# === AssemblyScript (uses @assemblyscript/wasi-shim for WASI support) ===
build-assemblyscript: $(OUT_DIR)
	@echo "Building AssemblyScript..."
	cd assemblyscript && npm install
	cd assemblyscript && npx asc hello_world.ts --config ./node_modules/@assemblyscript/wasi-shim/asconfig.json -o ../$(OUT_DIR)/hello_world_assemblyscript.wasm --optimize --optimizeLevel 3 --shrinkLevel 2
	cd assemblyscript && npx asc pi_approx.ts --config ./node_modules/@assemblyscript/wasi-shim/asconfig.json -o ../$(OUT_DIR)/pi_approx_assemblyscript.wasm --optimize --optimizeLevel 3 --shrinkLevel 2

# === Moonbit (uses peter-jerry-ye/wasi for WASI support) ===
build-moonbit: $(OUT_DIR)
	@echo "Building Moonbit..."
	cd moonbit/hello_world && moon build --target wasm --strip
	cd moonbit/pi_approx && moon build --target wasm --strip
	cp moonbit/hello_world/target/wasm/release/build/src/src.wasm $(OUT_DIR)/hello_world_moonbit.wasm
	cp moonbit/pi_approx/target/wasm/release/build/src/src.wasm $(OUT_DIR)/pi_approx_moonbit.wasm

# === Wado (component model - wasm-opt not supported) ===
build-wado: $(OUT_DIR)
	@echo "Building Wado..."
	@if [ -f wado/hello_world.wado ]; then \
		cargo run --manifest-path ../wado-cli/Cargo.toml -- compile -Os -o $(OUT_DIR)/hello_world_wado.wasm wado/hello_world.wado; \
		cargo run --manifest-path ../wado-cli/Cargo.toml -- compile -Os -o $(OUT_DIR)/pi_approx_wado.wasm wado/pi_approx.wado; \
	else \
		echo "SKIP: Wado (source files not found)"; \
	fi

# === Validation ===
validate:
	@echo ""
	@echo "=== Validation (running with wasmtime) ==="
	@for f in $(OUT_DIR)/*_rust.wasm $(OUT_DIR)/*_zig.wasm $(OUT_DIR)/*_tinygo.wasm $(OUT_DIR)/*_c.wasm $(OUT_DIR)/*_assemblyscript.wasm $(OUT_DIR)/*_moonbit.wasm; do \
		if [ -f "$$f" ]; then \
			printf "%-40s " "$$f:"; \
			$(WASMTIME) "$$f" 2>&1 | head -1 || echo "(failed)"; \
		fi; \
	done
	@# Wado uses WASI P3 component model
	@for f in $(OUT_DIR)/*_wado.wasm; do \
		if [ -f "$$f" ]; then \
			printf "%-40s " "$$f:"; \
			$(WASMTIME) run -S p3=y -W gc=y -W function-references=y -W component-model-async=y -W component-model-async-stackful=y --invoke 'run()' "$$f" 2>&1 | head -1 || echo "(failed)"; \
		fi; \
	done

# === Size Report ===
report-wasm-size: build-rust build-zig build-tinygo build-assemblyscript build-moonbit build-c build-wado validate
	@echo ""
	@echo "=== Wasm Size Report ==="
	@echo ""
	@echo "| Program      | Language       | Size (bytes) |"
	@echo "|--------------|----------------|--------------|"
	@for prog in $(PROGRAMS); do \
		for f in $(OUT_DIR)/$${prog}_*.wasm; do \
			if [ -f "$$f" ]; then \
				lang=$$(basename "$$f" .wasm | sed "s/$${prog}_//"); \
				size=$$(wc -c < "$$f" | tr -d ' '); \
				printf "%s\t%s\t%s\n" "$$size" "$$prog" "$$lang"; \
			fi; \
		done | sort -n | while IFS=$$'\t' read -r size prog lang; do \
			printf "| %-12s | %-14s | %12s |\n" "$$prog" "$$lang" "$$size"; \
		done; \
	done
	@echo ""

clean:
	rm -rf $(OUT_DIR)
	rm -rf rust/hello_world/target rust/pi_approx/target
	rm -rf moonbit/hello_world/target moonbit/pi_approx/target
	rm -rf assemblyscript/node_modules

install-deps:
	@echo "Install dependencies (macOS with Homebrew):"
	@echo "  brew install llvm lld wasi-libc wasi-runtimes  # C"
	@echo "  rustup target add wasm32-wasip1                # Rust"
	@echo "  brew install zig                               # Zig"
	@echo "  brew install tinygo                            # TinyGo"
	@echo "  brew install node                              # AssemblyScript"
	@echo "  curl -fsSL https://cli.moonbitlang.com/install/unix.sh | bash  # Moonbit"
