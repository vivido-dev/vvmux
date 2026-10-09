# vvmux-plugin-api

The stable contract between [vvmux](https://github.com/vivido-dev/vvmux) and its plugins.

- **Manifest** — the `vvmux-plugin.toml` types (`Manifest`, `Action`, `Pane`, `Workflow`,
  `Agent`, `Integration`, …) and `LoadedManifest::load`, which validates a package exactly as the
  host does, including its JSON Schemas.
- **Native protocol** — the length-prefixed JSON messages (`NativeMessage`, `NativeReply`) that a
  trusted native plugin service exchanges with vvmux over stdin and stdout, with `read_frame` and
  `write_frame`.
- **Component world** — `COMPONENT_WIT`, the WIT world that sandboxed WebAssembly component
  plugins implement.

Plugin authors writing Rust usually depend on
[`vvmux-plugin-sdk`](../vvmux-plugin-sdk), which re-exports this crate and adds a service loop and
component bindings.

Every list and string in a manifest is bounded by a `MAX_*` constant. Raising a bound lets newer
packages fail on older hosts, so the bounds change only with a new manifest or protocol version.

Licensed under Apache-2.0.
