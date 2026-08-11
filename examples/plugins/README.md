# vvmux reference plugins

- `rust-component`: sandboxed WebAssembly Component greeting action.
- `python-dashboard`: trusted one-shot action plus a real PTY dashboard.
- `typescript-agent`: trusted Node action authored in TypeScript, with an argv-only test runner.
- `vivid-chart`: trusted chart action and a pane that sends media through release-matched Vivi.
- `verification-bundle`: TOML-only workflow composing the native utilities.

Native packages run with the user's OS authority. Only the Rust Component is a
sandbox. Every action has Draft 2020-12 input/output schemas and is discoverable
through `vvmux plugin catalog --json` after installation.
