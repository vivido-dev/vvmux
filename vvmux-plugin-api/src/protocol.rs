use std::fmt;
use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PROTOCOL_VERSION: u16 = 1;
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Hello {
    pub protocol_version: u16,
    pub plugin_id: String,
    pub instance_id: String,
    #[serde(default)]
    pub features: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InvocationContext {
    pub correlation_id: String,
    pub causation_id: String,
    pub causation_depth: u8,
    pub source: String,
    pub session_instance: String,
    pub pane_id: Option<u64>,
    pub tab_id: Option<u64>,
    pub deadline_unix_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Invocation {
    pub request_id: u64,
    pub action: String,
    pub input: Value,
    pub context: InvocationContext,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Event {
    pub request_id: u64,
    pub sequence: u64,
    pub name: String,
    pub payload: Value,
    pub context: InvocationContext,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct HostCall {
    pub request_id: u64,
    pub method: String,
    pub params: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum NativeMessage {
    Hello(Hello),
    Initialize {
        request_id: u64,
    },
    Invoke(Invocation),
    Event(Event),
    Cancel {
        request_id: u64,
    },
    /// Reserved legacy direction; protocol-1 SDKs do not send host calls this way.
    HostCall(HostCall),
    /// Result of a host call initiated by the plugin on the reply stream.
    HostCallResult(HostCallResult),
    /// Typed failure of a host call initiated by the plugin.
    HostCallError(PluginError),
    Shutdown {
        request_id: u64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ResultEnvelope {
    pub request_id: u64,
    pub result: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct HostCallResult {
    pub request_id: u64,
    pub result: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PluginError {
    pub request_id: u64,
    pub code: ErrorCode,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum NativeReply {
    Hello(Hello),
    Ready {
        request_id: u64,
    },
    Result(ResultEnvelope),
    /// A plugin-to-host call. The host answers on the message stream with the same request ID.
    HostCall(HostCall),
    /// Reserved legacy direction; protocol-1 hosts answer on `NativeMessage` instead.
    HostCallResult(HostCallResult),
    Error(PluginError),
    Cancelled {
        request_id: u64,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    PluginNotFound,
    PluginDisabled,
    ActionNotFound,
    SchemaInvalid,
    CapabilityDenied,
    ScopeDenied,
    RuntimeUnavailable,
    RuntimeCrashed,
    Busy,
    Timeout,
    Cancelled,
    EventGap,
    DependencyFailed,
    OutputInvalid,
    ProtocolError,
}

#[derive(Debug)]
pub enum FrameError {
    Io(io::Error),
    TooLarge(usize),
    Json(serde_json::Error),
}

impl fmt::Display for FrameError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "{error}"),
            Self::TooLarge(size) => {
                write!(formatter, "plugin frame is {size} bytes; limit is 1 MiB")
            }
            Self::Json(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for FrameError {}

pub fn write_frame<T: Serialize>(mut writer: impl Write, value: &T) -> Result<(), FrameError> {
    let body = serde_json::to_vec(value).map_err(FrameError::Json)?;
    if body.len() > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge(body.len()));
    }
    writer
        .write_all(&(body.len() as u32).to_be_bytes())
        .and_then(|()| writer.write_all(&body))
        .and_then(|()| writer.flush())
        .map_err(FrameError::Io)
}

pub fn read_frame<T: for<'de> Deserialize<'de>>(mut reader: impl Read) -> Result<T, FrameError> {
    let mut prefix = [0_u8; 4];
    reader.read_exact(&mut prefix).map_err(FrameError::Io)?;
    let length = u32::from_be_bytes(prefix) as usize;
    if length > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge(length));
    }
    let mut body = vec![0_u8; length];
    reader.read_exact(&mut body).map_err(FrameError::Io)?;
    serde_json::from_slice(&body).map_err(FrameError::Json)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_round_trip_uses_big_endian_length() {
        let value = NativeReply::Ready { request_id: 7 };
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &value).unwrap();
        assert_eq!(
            u32::from_be_bytes(bytes[..4].try_into().unwrap()) as usize,
            bytes.len() - 4
        );
        assert_eq!(read_frame::<NativeReply>(&bytes[..]).unwrap(), value);
    }

    #[test]
    fn oversized_prefix_is_rejected_before_allocation() {
        let bytes = ((MAX_FRAME_BYTES + 1) as u32).to_be_bytes();
        assert!(matches!(
            read_frame::<Value>(&bytes[..]),
            Err(FrameError::TooLarge(_))
        ));
    }

    #[test]
    fn host_calls_use_plugin_reply_and_host_message_directions() {
        let call = HostCall {
            request_id: 11,
            method: "session.inspect".into(),
            params: serde_json::json!({}),
        };
        let mut plugin_bytes = Vec::new();
        write_frame(&mut plugin_bytes, &NativeReply::HostCall(call.clone())).unwrap();
        assert_eq!(
            read_frame::<NativeReply>(&plugin_bytes[..]).unwrap(),
            NativeReply::HostCall(call)
        );

        let result = HostCallResult {
            request_id: 11,
            result: serde_json::json!({"session": "test"}),
        };
        let mut host_bytes = Vec::new();
        write_frame(
            &mut host_bytes,
            &NativeMessage::HostCallResult(result.clone()),
        )
        .unwrap();
        assert_eq!(
            read_frame::<NativeMessage>(&host_bytes[..]).unwrap(),
            NativeMessage::HostCallResult(result)
        );
    }
}
