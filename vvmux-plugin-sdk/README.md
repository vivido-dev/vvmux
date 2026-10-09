# vvmux-plugin-sdk

Rust SDK for [vvmux](https://github.com/vivido-dev/vvmux) plugins.

- **Native services** (`[runtime] kind = "process"`): implement `Service` (or pass a closure) and
  call `serve_service`. The SDK runs the handshake, dispatches invocations and events serially,
  answers cancellation and shutdown, and lets handlers call back into the session through
  `NativeHost`. `serve_service_on` runs the same loop over any reader and writer, which makes a
  plugin's handlers testable in memory.
- **WebAssembly components** (`kind = "component"`, `wasm32-wasip2`): implement
  `component::Guest` and export it with `component::export!`.

The protocol and manifest types are re-exported from
[`vvmux-plugin-api`](../vvmux-plugin-api), so a plugin needs only this crate.

```rust,no_run
use vvmux_plugin_sdk::{Hello, Invocation, NativeHost, PROTOCOL_VERSION, PluginError, serve_service};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let hello = Hello {
        protocol_version: PROTOCOL_VERSION,
        plugin_id: std::env::var("VVMUX_PLUGIN_ID")?,
        instance_id: std::env::var("VVMUX_PLUGIN_INSTANCE")?,
        features: Vec::new(),
    };
    serve_service(hello, |invocation: Invocation, _host: &mut NativeHost<'_>| {
        Ok::<_, PluginError>(serde_json::json!({ "echo": invocation.input }))
    })?;
    Ok(())
}
```

Licensed under Apache-2.0.
