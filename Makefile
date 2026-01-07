.PHONY: build hello test clean

build:
	cd wado-compiler && cargo build

hello: build
	cd wado-compiler && cargo run --quiet -- ../example/hello.wado

test:
	cd wado-compiler && cargo test

clean:
	cd wado-compiler && cargo clean
