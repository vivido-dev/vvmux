//! Helpers for implementing native vvmux plugin services.

use std::io::{self, BufReader, BufWriter, Read, Write};

pub use vvmux_plugin_api::*;

/// Guest bindings and small JSON helpers for Rust WebAssembly Component authors.
///
/// Component crates implement [`component::Guest`] and export the implementation with
/// `component::export!(Type with_types_in component)`. The generated ABI is the same WIT world
/// that the host exposes; plugin code never speaks private VVMX.
#[cfg(target_arch = "wasm32")]
pub mod component {
    // `generate!` resolves `path` against this package's own directory, and `cargo package` never
    // carries files from a sibling package into the tarball, so the world cannot be read out of
    // `vvmux-plugin-api/`. The canonical world stays in that crate, published as
    // `vvmux_plugin_api::COMPONENT_WIT`; this mirror is proved byte-identical to it by
    // `wit_mirror_matches_the_published_world`.
    wit_bindgen::generate!({
        path: "wit",
        world: "plugin",
        pub_export_macro: true,
    });

    pub use exports::vivido::vvmux_plugin::guest::Guest;
    pub use vivido::vvmux_plugin::host::PluginError;

    /// Call a capability-checked method on the owning session with a JSON value.
    pub fn call(
        method: &str,
        params: &serde_json::Value,
    ) -> Result<serde_json::Value, PluginError> {
        let input = serde_json::to_vec(params).map_err(json_error)?;
        let output = vivido::vvmux_plugin::host::call(method, &input)?;
        serde_json::from_slice(&output).map_err(json_error)
    }

    /// Read one plugin-owned durable value.
    pub fn storage_get(key: &str) -> Result<Option<Vec<u8>>, PluginError> {
        vivido::vvmux_plugin::host::storage_get(key)
    }

    /// Atomically replace one plugin-owned durable value.
    pub fn storage_set(key: &str, value: &[u8]) -> Result<(), PluginError> {
        vivido::vvmux_plugin::host::storage_set(key, value)
    }

    /// Emit one bounded host-managed log entry.
    pub fn log(level: &str, message: &str) {
        vivido::vvmux_plugin::host::log(level, message);
    }

    /// Serialize a successful guest result without exposing generated ABI details.
    pub fn json(value: &serde_json::Value) -> Result<Vec<u8>, PluginError> {
        serde_json::to_vec(value).map_err(json_error)
    }

    /// Parse a bounded JSON invocation or context value.
    pub fn parse_json(bytes: &[u8]) -> Result<serde_json::Value, PluginError> {
        serde_json::from_slice(bytes).map_err(json_error)
    }

    /// Construct a stable typed guest error.
    pub fn error(code: &str, message: impl Into<String>) -> PluginError {
        PluginError {
            code: code.to_owned(),
            message: message.into(),
        }
    }

    fn json_error(error: serde_json::Error) -> PluginError {
        self::error("schema_invalid", error.to_string())
    }
}

/// Run a deterministic native service loop on stdin/stdout.
///
/// The host multiplexes request IDs, but calls this handler serially for one plugin instance.
pub fn serve(
    hello: Hello,
    mut handler: impl FnMut(Invocation) -> Result<serde_json::Value, PluginError>,
) -> Result<(), Box<dyn std::error::Error>> {
    serve_with_host(hello, move |invocation, _host| handler(invocation))
}

/// Run a native service with serialized action and event handlers.
pub fn serve_with_events(
    hello: Hello,
    mut handler: impl FnMut(Invocation) -> Result<serde_json::Value, PluginError>,
    mut event_handler: impl FnMut(Event) -> Result<(), PluginError>,
) -> Result<(), Box<dyn std::error::Error>> {
    serve_with_host_and_events(
        hello,
        move |invocation, _host| handler(invocation),
        move |event, _host| event_handler(event),
    )
}

/// A scoped client for brokered calls back into the owning vvmux session.
pub struct NativeHost<'a> {
    reader: &'a mut dyn Read,
    writer: &'a mut dyn Write,
    next_request_id: u64,
}

impl NativeHost<'_> {
    pub fn call(
        &mut self,
        method: impl Into<String>,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, PluginError> {
        let request_id = self.next_request_id;
        self.next_request_id = self.next_request_id.wrapping_add(1).max(1);
        write_frame(
            &mut self.writer,
            &NativeReply::HostCall(HostCall {
                request_id,
                method: method.into(),
                params,
            }),
        )
        .map_err(|error| PluginError {
            request_id,
            code: ErrorCode::ProtocolError,
            message: error.to_string(),
        })?;
        match read_frame::<NativeMessage>(&mut self.reader) {
            Ok(NativeMessage::HostCallResult(result)) if result.request_id == request_id => {
                Ok(result.result)
            }
            Ok(NativeMessage::HostCallError(error)) if error.request_id == request_id => Err(error),
            Ok(_) => Err(PluginError {
                request_id,
                code: ErrorCode::ProtocolError,
                message: "unexpected reply to native host call".into(),
            }),
            Err(error) => Err(PluginError {
                request_id,
                code: ErrorCode::ProtocolError,
                message: error.to_string(),
            }),
        }
    }
}

/// Serve serialized invocations whose handlers may make brokered host calls.
pub fn serve_with_host(
    hello: Hello,
    handler: impl FnMut(Invocation, &mut NativeHost<'_>) -> Result<serde_json::Value, PluginError>,
) -> Result<(), Box<dyn std::error::Error>> {
    serve_with_host_and_events(hello, handler, |_event, _host| Ok(()))
}

/// Serve serialized actions and events whose handlers may make brokered host calls.
pub fn serve_with_host_and_events(
    hello: Hello,
    mut handler: impl FnMut(Invocation, &mut NativeHost<'_>) -> Result<serde_json::Value, PluginError>,
    mut event_handler: impl FnMut(Event, &mut NativeHost<'_>) -> Result<(), PluginError>,
) -> Result<(), Box<dyn std::error::Error>> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut reader = BufReader::new(stdin.lock());
    let mut writer = BufWriter::new(stdout.lock());
    write_frame(&mut writer, &NativeReply::Hello(hello))?;
    loop {
        match read_frame::<NativeMessage>(&mut reader)? {
            NativeMessage::Initialize { request_id } => {
                write_frame(&mut writer, &NativeReply::Ready { request_id })?;
            }
            NativeMessage::Invoke(invocation) => {
                let request_id = invocation.request_id;
                let mut host = NativeHost {
                    reader: &mut reader,
                    writer: &mut writer,
                    next_request_id: 1,
                };
                let reply = match handler(invocation, &mut host) {
                    Ok(result) => NativeReply::Result(ResultEnvelope { request_id, result }),
                    Err(error) => NativeReply::Error(error),
                };
                write_frame(&mut writer, &reply)?;
            }
            NativeMessage::Event(event) => {
                let request_id = event.request_id;
                let mut host = NativeHost {
                    reader: &mut reader,
                    writer: &mut writer,
                    next_request_id: 1,
                };
                let reply = match event_handler(event, &mut host) {
                    Ok(()) => NativeReply::Ready { request_id },
                    Err(error) => NativeReply::Error(error),
                };
                write_frame(&mut writer, &reply)?;
            }
            NativeMessage::Cancel { request_id } => {
                write_frame(&mut writer, &NativeReply::Cancelled { request_id })?;
            }
            NativeMessage::Shutdown { request_id } => {
                write_frame(&mut writer, &NativeReply::Ready { request_id })?;
                return Ok(());
            }
            NativeMessage::Hello(_)
            | NativeMessage::HostCall(_)
            | NativeMessage::HostCallResult(_)
            | NativeMessage::HostCallError(_) => {
                return Err("unexpected native plugin message".into());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    /// The guest bindings are generated from a package-local copy, because a published tarball
    /// cannot reach into `vvmux-plugin-api/`. A drifted copy would let a plugin and its host
    /// speak different worlds, so the copy is not allowed to differ by a byte.
    #[test]
    fn wit_mirror_matches_the_published_world() {
        assert_eq!(
            include_str!("../wit/vvmux-plugin.wit"),
            vvmux_plugin_api::COMPONENT_WIT,
            "wit/vvmux-plugin.wit drifted from vvmux-plugin-api/wit/vvmux-plugin.wit"
        );
    }

    #[test]
    fn native_host_correlates_broker_calls() {
        let mut reply = Vec::new();
        write_frame(
            &mut reply,
            &NativeMessage::HostCallResult(HostCallResult {
                request_id: 1,
                result: serde_json::json!({"ok": true}),
            }),
        )
        .unwrap();
        let mut reader = Cursor::new(reply);
        let mut writer = Vec::new();
        let mut host = NativeHost {
            reader: &mut reader,
            writer: &mut writer,
            next_request_id: 1,
        };
        assert_eq!(
            host.call("session.inspect", serde_json::json!({})).unwrap(),
            serde_json::json!({"ok": true})
        );
        assert!(matches!(
            read_frame::<NativeReply>(&writer[..]).unwrap(),
            NativeReply::HostCall(HostCall { request_id: 1, .. })
        ));
    }
}
