.PHONY: build test

build:
	wasm-pack build core-rs --target web --out-dir ../app/src/wasm
	npm --prefix app run build

test:
	cargo test --manifest-path core-rs/Cargo.toml

