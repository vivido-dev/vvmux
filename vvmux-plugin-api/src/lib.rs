//! Stable, renderer-independent contracts shared by the vvmux plugin host and SDKs.
//!
//! This crate deliberately contains no VVMX types. VVMX is private session transport; plugin
//! authors use this protocol (native services) or the WIT world (WebAssembly Components).

mod manifest;
mod protocol;

pub use manifest::{
    Action, Activation, Dependency, EventHook, LoadedManifest, Manifest, ManifestError, Pane,
    Permission, Placement, Plugin, Runtime, RuntimeKind, SchemaDocument, Workflow, WorkflowStep,
};
pub use protocol::{
    ErrorCode, Event, FrameError, Hello, HostCall, HostCallResult, Invocation, InvocationContext,
    NativeMessage, NativeReply, PROTOCOL_VERSION, PluginError, ResultEnvelope, read_frame,
    write_frame,
};

/// Canonical component interface implemented by sandboxed plugins.
pub const COMPONENT_WIT: &str = include_str!("../wit/vvmux-plugin.wit");
