//! The native plugin protocol: length-prefixed JSON messages over a plugin's stdin and stdout.
//!
//! A native plugin service is a child process that vvmux starts and talks to over its standard
//! streams. Every message is one *frame*: a 4-byte big-endian length followed by that many bytes of
//! UTF-8 JSON. [`write_frame`] and [`read_frame`] implement the framing and enforce
//! [`MAX_FRAME_BYTES`] before allocating.
//!
//! Messages from vvmux to the plugin are [`NativeMessage`]s; messages from the plugin to vvmux are
//! [`NativeReply`]s. Both are internally tagged by a snake-case `type` field and reject unknown
//! fields. A session proceeds as follows:
//!
//! 1. The plugin sends [`NativeReply::Hello`] with [`PROTOCOL_VERSION`] and the plugin and
//!    instance IDs vvmux gave it in `VVMUX_PLUGIN_ID` and `VVMUX_PLUGIN_INSTANCE`.
//! 2. vvmux sends [`NativeMessage::Initialize`]; the plugin answers [`NativeReply::Ready`].
//! 3. For each [`NativeMessage::Invoke`] or [`NativeMessage::Event`], the plugin answers with a
//!    [`NativeReply::Result`] (or `Ready` for an event) or a [`NativeReply::Error`] carrying the
//!    same request ID. While handling one, it may send [`NativeReply::HostCall`]s and read the
//!    matching [`NativeMessage::HostCallResult`] or [`NativeMessage::HostCallError`].
//! 4. [`NativeMessage::Cancel`] asks the plugin to abandon a request and reply
//!    [`NativeReply::Cancelled`]; [`NativeMessage::Shutdown`] asks it to answer `Ready` and exit.
//!
//! vvmux sends requests to one plugin instance serially, so a plugin never has two invocations in
//! flight. Request IDs are chosen by the sender and only need to be unique among its outstanding
//! requests.
//!
//! # Examples
//!
//! ```
//! use vvmux_plugin_api::{NativeReply, read_frame, write_frame};
//!
//! let mut wire = Vec::new();
//! write_frame(&mut wire, &NativeReply::Ready { request_id: 1 })?;
//! assert_eq!(&wire[..4], &(wire.len() as u32 - 4).to_be_bytes());
//!
//! let reply: NativeReply = read_frame(wire.as_slice())?;
//! assert_eq!(reply, NativeReply::Ready { request_id: 1 });
//! # Ok::<(), vvmux_plugin_api::FrameError>(())
//! ```

use std::fmt;
use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The protocol version a plugin must announce in its [`Hello`].
pub const PROTOCOL_VERSION: u16 = 1;

/// Largest frame body, in bytes, that either side may send.
///
/// 1 MiB comfortably fits action inputs and results, which are bounded JSON documents; larger
/// payloads such as media travel through the Vivid capability path instead. The limit is checked
/// before the body is allocated, so a corrupt length prefix cannot exhaust memory.
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

/// The first message a plugin sends, identifying itself to vvmux.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Hello {
    /// Must equal [`PROTOCOL_VERSION`].
    pub protocol_version: u16,
    /// The plugin ID from the manifest, as given in `VVMUX_PLUGIN_ID`.
    pub plugin_id: String,
    /// This runtime's instance ID, as given in `VVMUX_PLUGIN_INSTANCE`.
    pub instance_id: String,
    /// Optional feature names; protocol-1 hosts ignore them.
    #[serde(default)]
    pub features: Vec<String>,
}

/// Where an invocation or event came from, and how long it may run.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InvocationContext {
    /// Identifies the whole chain of work that this request belongs to.
    pub correlation_id: String,
    /// Identifies the request that directly caused this one.
    pub causation_id: String,
    /// How many plugin-triggered hops led here; vvmux stops dispatching at depth eight.
    pub causation_depth: u8,
    /// The originator: `session`, or `plugin:<id>:<instance>` for plugin-initiated work.
    pub source: String,
    /// The identity of the session the request runs in.
    pub session_instance: String,
    /// The pane the request targets, when it has one.
    pub pane_id: Option<u64>,
    /// The tab the request targets, when it has one.
    pub tab_id: Option<u64>,
    /// Wall-clock deadline in milliseconds since the Unix epoch, or `0` for none.
    pub deadline_unix_ms: u64,
}

/// A request to run one of the plugin's manifest actions.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Invocation {
    /// The ID that the plugin's reply must carry.
    pub request_id: u64,
    /// The manifest action ID.
    pub action: String,
    /// The action input, already validated against the action's input schema.
    pub input: Value,
    /// Origin and deadline of the request.
    pub context: InvocationContext,
}

/// A session event delivered to a plugin that subscribed to it in its manifest.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Event {
    /// The ID that the plugin's acknowledgement must carry.
    pub request_id: u64,
    /// The session's monotonic event sequence number.
    pub sequence: u64,
    /// The event kind, one of [`EVENT_KINDS`](crate::EVENT_KINDS).
    pub name: String,
    /// The event's metadata payload.
    pub payload: Value,
    /// Origin of the event.
    pub context: InvocationContext,
}

/// A brokered call from a plugin to its owning session, such as `session.inspect`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct HostCall {
    /// The ID that vvmux's answer will carry.
    pub request_id: u64,
    /// The host method name.
    pub method: String,
    /// The method's parameters.
    pub params: Value,
}

/// A message from vvmux to a native plugin.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum NativeMessage {
    /// Reserved: vvmux does not send its own hello in protocol 1.
    Hello(Hello),
    /// Sent after the plugin's hello; answer with [`NativeReply::Ready`].
    Initialize {
        /// The ID that the `Ready` reply must carry.
        request_id: u64,
    },
    /// Run an action.
    Invoke(Invocation),
    /// Deliver a subscribed event.
    Event(Event),
    /// Abandon the request with this ID and reply [`NativeReply::Cancelled`].
    Cancel {
        /// The request to abandon.
        request_id: u64,
    },
    /// Reserved legacy direction; protocol-1 SDKs do not send host calls this way.
    HostCall(HostCall),
    /// Result of a host call initiated by the plugin on the reply stream.
    HostCallResult(HostCallResult),
    /// Typed failure of a host call initiated by the plugin.
    HostCallError(PluginError),
    /// Reply [`NativeReply::Ready`] and exit.
    Shutdown {
        /// The ID that the `Ready` reply must carry.
        request_id: u64,
    },
}

/// The successful result of an [`Invocation`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ResultEnvelope {
    /// The invocation's request ID.
    pub request_id: u64,
    /// The action output, which vvmux validates against the action's output schema.
    pub result: Value,
}

/// The successful result of a [`HostCall`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct HostCallResult {
    /// The host call's request ID.
    pub request_id: u64,
    /// The method's result.
    pub result: Value,
}

/// A typed failure of a request, sent in either direction.
///
/// This is a wire value rather than a Rust error chain: it carries no source error, and its
/// message is shown to users and agents, so it must not contain secrets.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PluginError {
    /// The ID of the failed request.
    pub request_id: u64,
    /// The machine-readable failure class.
    pub code: ErrorCode,
    /// A human-readable description.
    pub message: String,
}

impl fmt::Display for PluginError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for PluginError {}

/// A message from a native plugin to vvmux.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum NativeReply {
    /// The plugin's first message.
    Hello(Hello),
    /// Acknowledges `Initialize`, an event, or `Shutdown`.
    Ready {
        /// The acknowledged request's ID.
        request_id: u64,
    },
    /// The successful result of an invocation.
    Result(ResultEnvelope),
    /// A plugin-to-host call. The host answers on the message stream with the same request ID.
    HostCall(HostCall),
    /// Reserved legacy direction; protocol-1 hosts answer on `NativeMessage` instead.
    HostCallResult(HostCallResult),
    /// A failed invocation or event.
    Error(PluginError),
    /// Acknowledges a `Cancel`.
    Cancelled {
        /// The cancelled request's ID.
        request_id: u64,
    },
}

/// The machine-readable class of a [`PluginError`], serialized in snake case.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// No installed plugin has the requested ID.
    PluginNotFound,
    /// The plugin is installed but disabled.
    PluginDisabled,
    /// The plugin has no action with the requested ID.
    ActionNotFound,
    /// Input or output did not match its schema, or was not valid JSON.
    SchemaInvalid,
    /// The caller lacks a permission the request needs.
    CapabilityDenied,
    /// The request targets a session, pane, or tab outside the caller's scope.
    ScopeDenied,
    /// The plugin's runtime could not be started.
    RuntimeUnavailable,
    /// The plugin's runtime exited or broke the protocol.
    RuntimeCrashed,
    /// The plugin or session cannot accept more work right now.
    Busy,
    /// The request passed its deadline.
    Timeout,
    /// The request was cancelled.
    Cancelled,
    /// Events were dropped before delivery, leaving a sequence gap.
    EventGap,
    /// A workflow dependency failed.
    DependencyFailed,
    /// The action produced output that failed validation.
    OutputInvalid,
    /// A message violated the protocol.
    ProtocolError,
}

impl ErrorCode {
    /// The snake-case wire name of this code, such as `schema_invalid`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PluginNotFound => "plugin_not_found",
            Self::PluginDisabled => "plugin_disabled",
            Self::ActionNotFound => "action_not_found",
            Self::SchemaInvalid => "schema_invalid",
            Self::CapabilityDenied => "capability_denied",
            Self::ScopeDenied => "scope_denied",
            Self::RuntimeUnavailable => "runtime_unavailable",
            Self::RuntimeCrashed => "runtime_crashed",
            Self::Busy => "busy",
            Self::Timeout => "timeout",
            Self::Cancelled => "cancelled",
            Self::EventGap => "event_gap",
            Self::DependencyFailed => "dependency_failed",
            Self::OutputInvalid => "output_invalid",
            Self::ProtocolError => "protocol_error",
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why a frame could not be written or read.
#[derive(Debug)]
pub enum FrameError {
    /// The underlying stream failed or ended mid-frame.
    Io(io::Error),
    /// The frame body exceeds [`MAX_FRAME_BYTES`]; carries the offending length.
    TooLarge(usize),
    /// The body was not valid JSON for the expected message type.
    Json(serde_json::Error),
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "plugin frame I/O failed: {error}"),
            Self::TooLarge(size) => {
                write!(
                    f,
                    "plugin frame is {size} bytes; limit is {MAX_FRAME_BYTES} bytes"
                )
            }
            Self::Json(error) => write!(f, "plugin frame is not valid JSON: {error}"),
        }
    }
}

impl std::error::Error for FrameError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Json(error) => Some(error),
            Self::TooLarge(_) => None,
        }
    }
}

impl From<io::Error> for FrameError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for FrameError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

/// Serialize `value` as JSON and write it to `writer` as one flushed frame.
///
/// # Errors
///
/// Returns [`FrameError::Json`] when `value` cannot be serialized, [`FrameError::TooLarge`] when
/// its JSON exceeds [`MAX_FRAME_BYTES`] (nothing is written), or [`FrameError::Io`] when writing
/// or flushing fails.
pub fn write_frame<T: Serialize>(mut writer: impl Write, value: &T) -> Result<(), FrameError> {
    let body = serde_json::to_vec(value)?;
    if body.len() > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge(body.len()));
    }
    writer.write_all(&(body.len() as u32).to_be_bytes())?;
    writer.write_all(&body)?;
    writer.flush()?;
    Ok(())
}

/// Read one frame from `reader` and deserialize its JSON body as `T`.
///
/// Blocks until a whole frame is available. The length prefix is checked against
/// [`MAX_FRAME_BYTES`] before the body is allocated.
///
/// # Errors
///
/// Returns [`FrameError::Io`] when the stream fails or ends mid-frame (an
/// [`io::ErrorKind::UnexpectedEof`] at a frame boundary means the peer closed the stream),
/// [`FrameError::TooLarge`] for an oversized length prefix, or [`FrameError::Json`] when the body
/// is not a valid `T`.
pub fn read_frame<T: for<'de> Deserialize<'de>>(mut reader: impl Read) -> Result<T, FrameError> {
    let mut prefix = [0_u8; 4];
    reader.read_exact(&mut prefix)?;
    let length = u32::from_be_bytes(prefix) as usize;
    if length > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge(length));
    }
    let mut body = vec![0_u8; length];
    reader.read_exact(&mut body)?;
    Ok(serde_json::from_slice(&body)?)
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
        assert_eq!(read_frame::<NativeReply>(&*bytes).unwrap(), value);
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
    fn frame_errors_keep_their_cause() {
        use std::error::Error as _;

        let truncated = read_frame::<Value>(&[0, 0, 0, 4, b'{'][..]).unwrap_err();
        assert!(matches!(truncated, FrameError::Io(_)));
        assert!(truncated.source().is_some());

        let invalid = read_frame::<Value>(&[0, 0, 0, 1, b'{'][..]).unwrap_err();
        assert!(matches!(invalid, FrameError::Json(_)));
        assert!(invalid.source().is_some());
    }

    #[test]
    fn error_codes_display_their_wire_names() {
        for code in [
            ErrorCode::SchemaInvalid,
            ErrorCode::EventGap,
            ErrorCode::ProtocolError,
        ] {
            let wire = serde_json::to_value(code).unwrap();
            assert_eq!(wire, serde_json::Value::String(code.to_string()));
        }
        let error = PluginError {
            request_id: 3,
            code: ErrorCode::Timeout,
            message: "too slow".into(),
        };
        assert_eq!(error.to_string(), "timeout: too slow");
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
            read_frame::<NativeReply>(&*plugin_bytes).unwrap(),
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
            read_frame::<NativeMessage>(&*host_bytes).unwrap(),
            NativeMessage::HostCallResult(result)
        );
    }
}
