//! Helpers for implementing native vvmux plugin services.

use std::io::{self, BufReader, BufWriter};

pub use vvmux_plugin_api::*;

/// Run a deterministic native service loop on stdin/stdout.
///
/// The host multiplexes request IDs, but calls this handler serially for one plugin instance.
pub fn serve(
    hello: Hello,
    mut handler: impl FnMut(Invocation) -> Result<serde_json::Value, PluginError>,
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
                let reply = match handler(invocation) {
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
            NativeMessage::Hello(_) | NativeMessage::Event(_) | NativeMessage::HostCall(_) => {
                return Err("unexpected native plugin message".into());
            }
        }
    }
}
