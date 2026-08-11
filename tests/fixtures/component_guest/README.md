# Rust Component conformance fixture

This package is intentionally outside the main Cargo workspace. It consumes only the public
`vvmux-plugin-sdk` Component bindings and produces a `wasm32-wasip2` Component used by
`tests/plugin_component_conformance.rs`.

Install the Rust target before running that integration or the full workspace gate:

```sh
rustup target add wasm32-wasip2
```

The integration builds the fixture with its committed lockfile and a separate target directory.
It exercises the real WIT ABI rather than calling private host implementation details.
