.PHONY: build hello run test clean

build:
	cd wado-compiler && cargo build

hello: build
	cd wado-compiler && cargo run --quiet -- ../example/hello.wado

run: hello
	wasmtime example/hello.wat

test:
	cd wado-compiler && cargo test

clean:
	cd wado-compiler && cargo clean
	rm -f example/*.wat
