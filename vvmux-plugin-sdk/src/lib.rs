//! Rust SDK for vvmux plugins: a native service loop and WebAssembly component bindings.
//!
//! A **native service** (`[runtime] kind = "process"`) is a child process that speaks the framed
//! JSON protocol from `vvmux-plugin-api` on its standard streams. Implement [`Service`] (or pass a
//! closure) and hand it to [`serve_service`]; the SDK performs the hello and initialize handshake,
//! dispatches invocations and events one at a time, answers cancellation and shutdown, and lets
//! handlers make brokered calls back into the session through [`NativeHost`].
//!
//! A **WebAssembly component** (`kind = "component"`, built for `wasm32-wasip2`) instead
//! implements the `component::Guest` trait, which is generated from the plugin WIT world and
//! available only on `wasm32` targets.
//!
//! The protocol types are re-exported from `vvmux-plugin-api`, so a plugin needs only this crate.
//!
//! # Examples
//!
//! Drive a service over in-memory streams, the same way a unit test of a plugin can:
//!
//! ```
//! use vvmux_plugin_sdk::{
//!     Hello, Invocation, InvocationContext, NativeHost, NativeMessage, NativeReply, PluginError,
//!     read_frame, serve_service_on, write_frame,
//! };
//!
//! let mut input = Vec::new();
//! write_frame(&mut input, &NativeMessage::Initialize { request_id: 1 })?;
//! write_frame(
//!     &mut input,
//!     &NativeMessage::Invoke(Invocation {
//!         request_id: 2,
//!         action: "greet".into(),
//!         input: serde_json::json!({ "name": "Ada" }),
//!         context: InvocationContext {
//!             correlation_id: "c".into(),
//!             causation_id: "c".into(),
//!             causation_depth: 0,
//!             source: "session".into(),
//!             session_instance: "s".into(),
//!             pane_id: None,
//!             tab_id: None,
//!             deadline_unix_ms: 0,
//!         },
//!     }),
//! )?;
//! write_frame(&mut input, &NativeMessage::Shutdown { request_id: 3 })?;
//!
//! let hello = Hello {
//!     protocol_version: vvmux_plugin_sdk::PROTOCOL_VERSION,
//!     plugin_id: "com.example.hello".into(),
//!     instance_id: "instance-1".into(),
//!     features: Vec::new(),
//! };
//! let mut output = Vec::new();
//! serve_service_on(
//!     input.as_slice(),
//!     &mut output,
//!     hello,
//!     |invocation: Invocation, _host: &mut NativeHost<'_>| -> Result<_, PluginError> {
//!         Ok(serde_json::json!({ "greeting": format!("Hello, {}!", invocation.input["name"]) }))
//!     },
//! )?;
//!
//! let mut replies = output.as_slice();
//! assert!(matches!(read_frame::<NativeReply>(&mut replies)?, NativeReply::Hello(_)));
//! assert!(matches!(read_frame::<NativeReply>(&mut replies)?, NativeReply::Ready { .. }));
//! let NativeReply::Result(result) = read_frame::<NativeReply>(&mut replies)? else {
//!     panic!("expected a result");
//! };
//! assert_eq!(result.result["greeting"], "Hello, \"Ada\"!");
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use std::backtrace::{Backtrace, BacktraceStatus};
use std::fmt;
use std::io::{self, BufReader, BufWriter, Read, Write};

pub use vvmux_plugin_api::{
    Action, Activation, Agent, AgentGate, AgentLaunch, AgentProcess, AgentRule, AgentRuleState,
    COMPONENT_WIT, ComponentPreopen, Dependency, EVENT_KINDS, ErrorCode, Event, EventHook,
    FrameError, Hello, HostCall, HostCallResult, Integration, IntegrationFile,
    IntegrationRegistration, Invocation, InvocationContext, Keybinding, LinkHandler,
    LoadedManifest, MAX_AGENT_ARGV_MARKERS, MAX_AGENT_EXECUTABLE_BYTES, MAX_AGENT_EXECUTABLES,
    MAX_AGENT_GATE_DEPTH, MAX_AGENT_MATCHER_BYTES, MAX_AGENT_RESUME_ARGS, MAX_AGENT_RULES,
    MAX_AGENTS_PER_PLUGIN, MAX_FRAME_BYTES, MAX_INTEGRATION_ARG_BYTES, MAX_INTEGRATION_ARGS,
    MAX_INTEGRATION_FILE_BYTES, MAX_INTEGRATION_FILES, MAX_INTEGRATION_NOTICE_BYTES,
    MAX_INTEGRATION_PATH_SEGMENTS, MAX_INTEGRATION_REGISTRATIONS, MAX_INTEGRATIONS_PER_PLUGIN,
    MAX_KEYBINDINGS_PER_PLUGIN, MAX_LINK_HANDLERS_PER_PLUGIN, MAX_LINK_PATTERN_BYTES,
    MAX_MANIFEST_BYTES, MAX_SCHEMA_BYTES, MAX_SCHEMA_DEPTH, MAX_STARTUP_TIMEOUT_MS,
    MAX_WORKFLOW_STEPS, MAX_WORKFLOWS, Manifest, ManifestError, NativeMessage, NativeReply,
    PROTOCOL_VERSION, Pane, Permission, Placement, Plugin, PluginError, RESUME_ID_PLACEHOLDER,
    RESUME_PATH_PLACEHOLDER, ResultEnvelope, Runtime, RuntimeKind, SchemaDocument, Workflow,
    WorkflowStep, read_frame, validate_schema_instance, write_frame,
};

/// Guest bindings and small JSON helpers for Rust WebAssembly Component authors.
///
/// Component crates implement [`component::Guest`] and export the implementation with
/// `component::export!(Type with_types_in component)`. The generated ABI is the same WIT world
/// that the host exposes; plugin code never speaks private VVMX.
#[cfg(target_arch = "wasm32")]
#[allow(
    clippy::same_length_and_capacity,
    reason = "raised inside code that `wit_bindgen::generate!` emits"
)]
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
    ///
    /// # Errors
    ///
    /// Returns the host's typed error when the call is refused or fails, or a `schema_invalid`
    /// error when the parameters or result are not valid JSON.
    pub fn call(
        method: &str,
        params: &serde_json::Value,
    ) -> Result<serde_json::Value, PluginError> {
        let input = serde_json::to_vec(params).map_err(json_error)?;
        let output = vivido::vvmux_plugin::host::call(method, &input)?;
        serde_json::from_slice(&output).map_err(json_error)
    }

    /// Read one plugin-owned durable value.
    ///
    /// # Errors
    ///
    /// Returns the host's typed error when storage is unavailable or the key is invalid.
    pub fn storage_get(key: &str) -> Result<Option<Vec<u8>>, PluginError> {
        vivido::vvmux_plugin::host::storage_get(key)
    }

    /// Atomically replace one plugin-owned durable value.
    ///
    /// # Errors
    ///
    /// Returns the host's typed error when storage is unavailable, the key is invalid, or the
    /// value exceeds the host's limit.
    pub fn storage_set(key: &str, value: &[u8]) -> Result<(), PluginError> {
        vivido::vvmux_plugin::host::storage_set(key, value)
    }

    /// Emit one bounded host-managed log entry.
    pub fn log(level: &str, message: &str) {
        vivido::vvmux_plugin::host::log(level, message);
    }

    /// Serialize a successful guest result without exposing generated ABI details.
    ///
    /// # Errors
    ///
    /// Returns a `schema_invalid` error when `value` cannot be serialized.
    pub fn json(value: &serde_json::Value) -> Result<Vec<u8>, PluginError> {
        serde_json::to_vec(value).map_err(json_error)
    }

    /// Parse a bounded JSON invocation or context value.
    ///
    /// # Errors
    ///
    /// Returns a `schema_invalid` error when `bytes` is not valid JSON.
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

/// A native plugin's request handlers.
///
/// vvmux calls one plugin instance serially, so neither method is ever re-entered. Both receive a
/// [`NativeHost`] for brokered calls back into the owning session during the request.
///
/// Any `FnMut(Invocation, &mut NativeHost) -> Result<Value, PluginError>` closure is a `Service`
/// that ignores events.
pub trait Service {
    /// Run one action and return its output, which vvmux validates against the output schema.
    ///
    /// # Errors
    ///
    /// Return a [`PluginError`] with the invocation's request ID to fail the action.
    fn invoke(
        &mut self,
        invocation: Invocation,
        host: &mut NativeHost<'_>,
    ) -> Result<serde_json::Value, PluginError>;

    /// Handle one subscribed event. The default ignores it.
    ///
    /// # Errors
    ///
    /// Return a [`PluginError`] with the event's request ID to report a failed hook.
    fn event(&mut self, event: Event, host: &mut NativeHost<'_>) -> Result<(), PluginError> {
        let _ = (event, host);
        Ok(())
    }
}

impl<F> Service for F
where
    F: FnMut(Invocation, &mut NativeHost<'_>) -> Result<serde_json::Value, PluginError>,
{
    fn invoke(
        &mut self,
        invocation: Invocation,
        host: &mut NativeHost<'_>,
    ) -> Result<serde_json::Value, PluginError> {
        self(invocation, host)
    }
}

/// Why a native service loop stopped with an error.
///
/// A host that closes the plugin's stdin ends the loop with an error for which
/// [`ServeError::is_closed`] is true.
#[derive(Debug)]
pub struct ServeError {
    kind: ServeErrorKind,
    backtrace: Backtrace,
}

#[derive(Debug)]
enum ServeErrorKind {
    Frame(FrameError),
    UnexpectedMessage,
}

impl ServeError {
    fn new(kind: ServeErrorKind) -> Self {
        Self {
            kind,
            backtrace: Backtrace::capture(),
        }
    }

    /// Whether the host closed the protocol stream.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        matches!(
            &self.kind,
            ServeErrorKind::Frame(FrameError::Io(error))
                if error.kind() == io::ErrorKind::UnexpectedEof
        )
    }

    /// Whether reading or writing the protocol stream failed.
    #[must_use]
    pub fn is_io(&self) -> bool {
        matches!(&self.kind, ServeErrorKind::Frame(FrameError::Io(_)))
    }

    /// Whether the host sent something the protocol does not allow: an invalid or oversized
    /// frame, or a message that only a plugin may send.
    #[must_use]
    pub fn is_protocol(&self) -> bool {
        matches!(
            &self.kind,
            ServeErrorKind::UnexpectedMessage
                | ServeErrorKind::Frame(FrameError::Json(_) | FrameError::TooLarge(_))
        )
    }
}

impl From<FrameError> for ServeError {
    fn from(error: FrameError) -> Self {
        Self::new(ServeErrorKind::Frame(error))
    }
}

impl fmt::Display for ServeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            ServeErrorKind::Frame(error) => write!(f, "native plugin service stopped: {error}")?,
            ServeErrorKind::UnexpectedMessage => f.write_str("unexpected native plugin message")?,
        }
        if self.backtrace.status() == BacktraceStatus::Captured {
            write!(f, "\n\nbacktrace:\n{}", self.backtrace)?;
        }
        Ok(())
    }
}

impl std::error::Error for ServeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.kind {
            ServeErrorKind::Frame(error) => Some(error),
            ServeErrorKind::UnexpectedMessage => None,
        }
    }
}

/// Run `service` as a native plugin on this process's stdin and stdout until shutdown.
///
/// # Errors
///
/// Returns a [`ServeError`] when the host closes the stream before `Shutdown`, a frame cannot be
/// read or written, or the host sends a message that only a plugin may send.
pub fn serve_service(hello: Hello, service: impl Service) -> Result<(), ServeError> {
    serve_service_on(io::stdin().lock(), io::stdout().lock(), hello, service)
}

/// Run `service` as a native plugin over `reader` and `writer` until shutdown.
///
/// This is [`serve_service`] with the streams supplied by the caller, which lets a plugin's own
/// tests drive it in memory. Both streams are buffered internally.
///
/// # Errors
///
/// The same as [`serve_service`].
pub fn serve_service_on(
    reader: impl Read,
    writer: impl Write,
    hello: Hello,
    mut service: impl Service,
) -> Result<(), ServeError> {
    let mut reader = BufReader::new(reader);
    let mut writer = BufWriter::new(writer);
    write_frame(&mut writer, &NativeReply::Hello(hello))?;
    loop {
        match read_frame::<NativeMessage>(&mut reader)? {
            NativeMessage::Initialize { request_id } => {
                write_frame(&mut writer, &NativeReply::Ready { request_id })?;
            }
            NativeMessage::Invoke(invocation) => {
                let request_id = invocation.request_id;
                let mut host = NativeHost::new(&mut reader, &mut writer);
                let reply = match service.invoke(invocation, &mut host) {
                    Ok(result) => NativeReply::Result(ResultEnvelope { request_id, result }),
                    Err(error) => NativeReply::Error(error),
                };
                write_frame(&mut writer, &reply)?;
            }
            NativeMessage::Event(event) => {
                let request_id = event.request_id;
                let mut host = NativeHost::new(&mut reader, &mut writer);
                let reply = match service.event(event, &mut host) {
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
                return Err(ServeError::new(ServeErrorKind::UnexpectedMessage));
            }
        }
    }
}

/// Adapts the earlier closure-pair entry points to [`Service`].
struct ClosureService<I, E> {
    invoke: I,
    event: E,
}

impl<I, E> Service for ClosureService<I, E>
where
    I: FnMut(Invocation, &mut NativeHost<'_>) -> Result<serde_json::Value, PluginError>,
    E: FnMut(Event, &mut NativeHost<'_>) -> Result<(), PluginError>,
{
    fn invoke(
        &mut self,
        invocation: Invocation,
        host: &mut NativeHost<'_>,
    ) -> Result<serde_json::Value, PluginError> {
        (self.invoke)(invocation, host)
    }

    fn event(&mut self, event: Event, host: &mut NativeHost<'_>) -> Result<(), PluginError> {
        (self.event)(event, host)
    }
}

/// Run a deterministic native service loop on stdin/stdout.
///
/// The host multiplexes request IDs, but calls this handler serially for one plugin instance.
/// Prefer [`serve_service`], which accepts any [`Service`] and returns a typed error.
///
/// # Errors
///
/// The same conditions as [`serve_service`], boxed.
pub fn serve(
    hello: Hello,
    mut handler: impl FnMut(Invocation) -> Result<serde_json::Value, PluginError>,
) -> Result<(), Box<dyn std::error::Error>> {
    serve_with_host(hello, move |invocation, _host| handler(invocation))
}

/// Run a native service with serialized action and event handlers.
///
/// Prefer implementing [`Service`] and calling [`serve_service`].
///
/// # Errors
///
/// The same conditions as [`serve_service`], boxed.
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
///
/// The SDK hands one to each handler call. [`NativeHost::new`] builds one over any streams, so a
/// handler can be unit tested against a scripted host.
pub struct NativeHost<'a> {
    reader: &'a mut dyn Read,
    writer: &'a mut dyn Write,
    next_request_id: u64,
}

impl fmt::Debug for NativeHost<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NativeHost")
            .field("next_request_id", &self.next_request_id)
            .finish_non_exhaustive()
    }
}

impl<'a> NativeHost<'a> {
    /// A host client that writes calls to `writer` and reads answers from `reader`.
    ///
    /// `reader` must yield the host's [`NativeMessage`] frames and `writer` accept
    /// [`NativeReply`] frames, as the plugin's stdin and stdout do.
    pub fn new(reader: &'a mut dyn Read, writer: &'a mut dyn Write) -> Self {
        Self {
            reader,
            writer,
            next_request_id: 1,
        }
    }

    /// Call `method` on the owning session and wait for its result.
    ///
    /// # Errors
    ///
    /// Returns the host's [`PluginError`] when the call is refused or fails, or a
    /// [`ErrorCode::ProtocolError`] when the stream fails or the host answers out of order.
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
///
/// Prefer [`serve_service`], which accepts the same closure.
///
/// # Errors
///
/// The same conditions as [`serve_service`], boxed.
pub fn serve_with_host(
    hello: Hello,
    handler: impl FnMut(Invocation, &mut NativeHost<'_>) -> Result<serde_json::Value, PluginError>,
) -> Result<(), Box<dyn std::error::Error>> {
    serve_with_host_and_events(hello, handler, |_event, _host| Ok(()))
}

/// Serve serialized actions and events whose handlers may make brokered host calls.
///
/// Prefer implementing [`Service`] and calling [`serve_service`].
///
/// # Errors
///
/// The same conditions as [`serve_service`], boxed.
pub fn serve_with_host_and_events(
    hello: Hello,
    handler: impl FnMut(Invocation, &mut NativeHost<'_>) -> Result<serde_json::Value, PluginError>,
    event_handler: impl FnMut(Event, &mut NativeHost<'_>) -> Result<(), PluginError>,
) -> Result<(), Box<dyn std::error::Error>> {
    serve_service(
        hello,
        ClosureService {
            invoke: handler,
            event: event_handler,
        },
    )
    .map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use std::error::Error as _;
    use std::io::Cursor;

    use super::*;

    fn hello() -> Hello {
        Hello {
            protocol_version: PROTOCOL_VERSION,
            plugin_id: "com.example.test".into(),
            instance_id: "instance".into(),
            features: Vec::new(),
        }
    }

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
        let mut host = NativeHost::new(&mut reader, &mut writer);
        assert_eq!(
            host.call("session.inspect", serde_json::json!({})).unwrap(),
            serde_json::json!({"ok": true})
        );
        assert!(matches!(
            read_frame::<NativeReply>(&*writer).unwrap(),
            NativeReply::HostCall(HostCall { request_id: 1, .. })
        ));
    }

    struct Counter<'a> {
        events: &'a std::cell::Cell<u64>,
    }

    impl Service for Counter<'_> {
        fn invoke(
            &mut self,
            invocation: Invocation,
            _host: &mut NativeHost<'_>,
        ) -> Result<serde_json::Value, PluginError> {
            Err(PluginError {
                request_id: invocation.request_id,
                code: ErrorCode::ActionNotFound,
                message: invocation.action,
            })
        }

        fn event(&mut self, event: Event, _host: &mut NativeHost<'_>) -> Result<(), PluginError> {
            self.events.set(self.events.get() + event.sequence);
            Ok(())
        }
    }

    fn context() -> InvocationContext {
        InvocationContext {
            correlation_id: "c".into(),
            causation_id: "c".into(),
            causation_depth: 0,
            source: "session".into(),
            session_instance: "s".into(),
            pane_id: None,
            tab_id: None,
            deadline_unix_ms: 0,
        }
    }

    #[test]
    fn a_service_answers_events_errors_cancellation_and_shutdown() {
        let mut input = Vec::new();
        for message in [
            NativeMessage::Initialize { request_id: 1 },
            NativeMessage::Event(Event {
                request_id: 2,
                sequence: 5,
                name: "pane.opened".into(),
                payload: serde_json::json!({}),
                context: context(),
            }),
            NativeMessage::Invoke(Invocation {
                request_id: 3,
                action: "missing".into(),
                input: serde_json::json!({}),
                context: context(),
            }),
            NativeMessage::Cancel { request_id: 4 },
            NativeMessage::Shutdown { request_id: 5 },
        ] {
            write_frame(&mut input, &message).unwrap();
        }
        let mut output = Vec::new();
        let events = std::cell::Cell::new(0);
        serve_service_on(
            input.as_slice(),
            &mut output,
            hello(),
            Counter { events: &events },
        )
        .unwrap();
        assert_eq!(events.get(), 5);

        let mut replies = output.as_slice();
        let mut next = || read_frame::<NativeReply>(&mut replies).unwrap();
        assert!(matches!(next(), NativeReply::Hello(_)));
        assert_eq!(next(), NativeReply::Ready { request_id: 1 });
        assert_eq!(next(), NativeReply::Ready { request_id: 2 });
        assert!(matches!(
            next(),
            NativeReply::Error(PluginError {
                request_id: 3,
                code: ErrorCode::ActionNotFound,
                ..
            })
        ));
        assert_eq!(next(), NativeReply::Cancelled { request_id: 4 });
        assert_eq!(next(), NativeReply::Ready { request_id: 5 });
    }

    #[test]
    fn a_closed_stream_and_a_plugin_only_message_are_distinguished() {
        let events = std::cell::Cell::new(0);
        let closed = serve_service_on(&[][..], Vec::new(), hello(), Counter { events: &events })
            .unwrap_err();
        assert!(closed.is_closed());
        assert!(closed.is_io());
        assert!(closed.source().is_some());

        let mut input = Vec::new();
        write_frame(&mut input, &NativeMessage::Hello(hello())).unwrap();
        let unexpected = serve_service_on(
            input.as_slice(),
            Vec::new(),
            hello(),
            Counter { events: &events },
        )
        .unwrap_err();
        assert!(unexpected.is_protocol());
        assert!(!unexpected.is_closed());
        assert!(
            unexpected
                .to_string()
                .starts_with("unexpected native plugin message")
        );
    }

    #[test]
    fn native_host_debug_shows_no_stream_contents() {
        let mut reader = Cursor::new(b"secret-token".to_vec());
        let mut writer = Vec::new();
        let host = NativeHost::new(&mut reader, &mut writer);
        let rendered = format!("{host:?}");
        assert!(rendered.contains("NativeHost"));
        assert!(!rendered.contains("secret-token"));
    }
}
