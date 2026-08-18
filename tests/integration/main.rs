//! Every integration test in one binary.
//!
//! `cargo test` runs test binaries one after another, so a target per file costs the sum of the
//! files' slowest tests — a bill dominated by end-to-end fixtures that spend their time waiting on
//! real sessions rather than on a CPU. Sharing one binary lets libtest schedule all of them across
//! its thread pool, which is also what makes the run agree with `cargo nextest run`. Each module
//! keeps the platform and feature gate it needs, so a target that cannot build a module still
//! builds the binary.

#[allow(dead_code)]
mod common;

mod automation_msg;
mod automation_run;
mod config_reload;
mod daemon_descriptors;
mod gateway_ws;
mod image_probe;
mod mouse_selection;
mod osc52_clipboard;
mod plugin_component_conformance;
mod plugin_events;
mod plugin_host_calls;
mod plugin_integration_cli;
mod plugin_panes;
mod plugin_reference_slice;
mod plugin_reload;
mod plugin_session_contract;
mod plugin_workflows;
mod search;
mod startup_layout;
mod sync_input;
mod tunnel_connect;
mod unix_focus_reporting;
mod unix_resize;
mod windows_automation;
mod windows_conpty_daemon;
mod windows_ctrl_c;
mod windows_ctrl_c_direct;
mod windows_reattach;
mod windows_resize;
