//! Helpers for implementing native vvmux plugin services.

use std::io::{self, BufReader, BufWriter, Read, Write};

pub use vvmux_plugin_api::*;

/// Run a deterministic native service loop on stdin/stdout.
///
/// The host multiplexes request IDs, but calls this handler serially for one plugin instance.
pub fn serve(
    hello: Hello,
    mut handler: impl FnMut(Invocation) -> Result<serde_json::Value, PluginError>,
) -> Result<(), Box<dyn std::error::Error>> {
    serve_with_host(hello, move |invocation, _host| handler(invocation))
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
    mut handler: impl FnMut(Invocation, &mut NativeHost<'_>) -> Result<serde_json::Value, PluginError>,
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
            NativeMessage::Cancel { request_id } => {
                write_frame(&mut writer, &NativeReply::Cancelled { request_id })?;
            }
            NativeMessage::Shutdown { request_id } => {
                write_frame(&mut writer, &NativeReply::Ready { request_id })?;
                return Ok(());
            }
            NativeMessage::Hello(_)
            | NativeMessage::Event(_)
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
