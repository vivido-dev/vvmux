//! Stable, renderer-independent contracts shared by the vvmux plugin host and SDKs.
//!
//! A vvmux plugin is a directory holding a `vvmux-plugin.toml` [`Manifest`], the JSON Schemas its
//! actions reference, and optionally a runtime. This crate defines both halves of the contract a
//! plugin author codes against:
//!
//! - **The manifest.** [`Manifest`] and its tables describe actions, event hooks, panes,
//!   workflows, agents, and integrations. [`LoadedManifest::load`] parses and validates a package
//!   exactly as the host does, so authors and tooling can check a package before installing it.
//! - **The native protocol.** A trusted native service (`kind = "process"`) talks to vvmux over
//!   its standard streams with length-prefixed JSON frames: [`NativeMessage`] from the host,
//!   [`NativeReply`] from the plugin, framed by [`read_frame`] and [`write_frame`]. The protocol
//!   module documentation walks through the message sequence.
//! - **The component world.** A sandboxed WebAssembly component (`kind = "component"`) implements
//!   the WIT world in [`COMPONENT_WIT`] instead.
//!
//! This crate deliberately contains no VVMX types. VVMX is the private session transport between
//! vvmux processes and is not a plugin contract.
//!
//! # Compatibility
//!
//! These types are exact contracts, not forward-compatible envelopes: manifests and frames parse
//! with `deny_unknown_fields`, and neither side ignores fields it does not know. Adding a field,
//! variant, or permission to a same-version shape is therefore a breaking change for Rust callers
//! and for older peers. Shape evolution goes through the existing version gates instead: new
//! manifest tables require a higher `manifest_version`, which older hosts reject up front, and
//! native-protocol changes require a [`PROTOCOL_VERSION`] bump. The `MAX_*` bounds are part of the
//! same contract — raising one lets newer packages fail on older hosts — so bound changes are
//! releases, not patches.
//!
//! # Examples
//!
//! Validate a manifest without touching the filesystem:
//!
//! ```
//! use vvmux_plugin_api::Manifest;
//!
//! let manifest: Manifest = toml::from_str(
//!     r#"
//!     manifest_version = 1
//!
//!     [plugin]
//!     id = "com.example.hello"
//!     name = "Hello"
//!     version = "0.1.0"
//!     min_vvmux_version = "0.5.0"
//!     description = "Greets the user."
//!     platforms = ["linux", "macos"]
//!
//!     [[actions]]
//!     id = "greet"
//!     title = "Greet"
//!     description = "Say hello."
//!     command = ["./greet.sh"]
//!     input_schema = "schemas/greet.input.json"
//!     output_schema = "schemas/greet.output.json"
//!     "#,
//! )?;
//! manifest.validate()?;
//! assert_eq!(manifest.actions[0].timeout_ms, 30_000);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

mod manifest;
mod protocol;

#[doc(inline)]
pub use manifest::{
    Action, Activation, Agent, AgentGate, AgentLaunch, AgentProcess, AgentRule, AgentRuleState,
    ComponentPreopen, Dependency, EVENT_KINDS, EventHook, Integration, IntegrationFile,
    IntegrationRegistration, Keybinding, LinkHandler, LoadedManifest, MAX_AGENT_ARGV_MARKERS,
    MAX_AGENT_EXECUTABLE_BYTES, MAX_AGENT_EXECUTABLES, MAX_AGENT_GATE_DEPTH,
    MAX_AGENT_MATCHER_BYTES, MAX_AGENT_RESUME_ARGS, MAX_AGENT_RULES, MAX_AGENTS_PER_PLUGIN,
    MAX_INTEGRATION_ARG_BYTES, MAX_INTEGRATION_ARGS, MAX_INTEGRATION_FILE_BYTES,
    MAX_INTEGRATION_FILES, MAX_INTEGRATION_NOTICE_BYTES, MAX_INTEGRATION_PATH_SEGMENTS,
    MAX_INTEGRATION_REGISTRATIONS, MAX_INTEGRATIONS_PER_PLUGIN, MAX_KEYBINDINGS_PER_PLUGIN,
    MAX_LINK_HANDLERS_PER_PLUGIN, MAX_LINK_PATTERN_BYTES, MAX_MANIFEST_BYTES, MAX_SCHEMA_BYTES,
    MAX_SCHEMA_DEPTH, MAX_STARTUP_TIMEOUT_MS, MAX_WORKFLOW_STEPS, MAX_WORKFLOWS, Manifest,
    ManifestError, Pane, Permission, Placement, Plugin, RESUME_ID_PLACEHOLDER,
    RESUME_PATH_PLACEHOLDER, Runtime, RuntimeKind, SchemaDocument, Workflow, WorkflowStep,
    validate_schema_instance,
};
#[doc(inline)]
pub use protocol::{
    ErrorCode, Event, FrameError, Hello, HostCall, HostCallResult, Invocation, InvocationContext,
    MAX_FRAME_BYTES, NativeMessage, NativeReply, PROTOCOL_VERSION, PluginError, ResultEnvelope,
    read_frame, write_frame,
};

/// Canonical component interface implemented by sandboxed plugins.
pub const COMPONENT_WIT: &str = include_str!("../wit/vvmux-plugin.wit");
