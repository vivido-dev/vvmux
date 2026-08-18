//! Stable, renderer-independent contracts shared by the vvmux plugin host and SDKs.
//!
//! This crate deliberately contains no VVMX types. VVMX is private session transport; plugin
//! authors use this protocol (native services) or the WIT world (WebAssembly Components).

mod manifest;
mod protocol;

pub use manifest::{
    Action, Activation, Agent, AgentGate, AgentProcess, AgentRule, AgentRuleState,
    ComponentPreopen, Dependency, EventHook, Integration, IntegrationFile, IntegrationRegistration,
    LoadedManifest, MAX_AGENT_RESUME_ARGS, MAX_INTEGRATION_FILE_BYTES, Manifest, ManifestError,
    Pane, Permission, Placement, Plugin, RESUME_ID_PLACEHOLDER, RESUME_PATH_PLACEHOLDER, Runtime,
    RuntimeKind, SchemaDocument, Workflow, WorkflowStep, validate_schema_instance,
};
pub use protocol::{
    ErrorCode, Event, FrameError, Hello, HostCall, HostCallResult, Invocation, InvocationContext,
    MAX_FRAME_BYTES, NativeMessage, NativeReply, PROTOCOL_VERSION, PluginError, ResultEnvelope,
    read_frame, write_frame,
};

/// Canonical component interface implemented by sandboxed plugins.
pub const COMPONENT_WIT: &str = include_str!("../wit/vvmux-plugin.wit");
