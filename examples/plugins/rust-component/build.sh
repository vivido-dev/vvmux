#!/bin/sh
set -eu
cargo build --locked --release --target wasm32-wasip2
mkdir -p dist
cp target/wasm32-wasip2/release/vvmux_reference_hello_component.wasm dist/hello.wasm
