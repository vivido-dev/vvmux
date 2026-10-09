//! Pane-oriented terminal emulation and platform PTY support for vvmux.
//!
//! [`Terminal`] is a headless terminal emulator: it parses a pane's output into a grid of
//! [`Cell`]s with scrollback, tracks the [`TerminalModes`] an application sets, and reports
//! [`TerminalEvent`]s, such as damage, titles, clipboard requests, and bytes to write back, for
//! its host to act on. It performs no I/O.
//!
//! Beyond VT parsing, it intercepts sequences a multiplexer must not pass through blindly:
//! Kitty graphics commands are validated and surfaced as [`KittyGraphicsCommand`]s, device-status
//! and capability queries are answered on behalf of the pane, Vivid anchor markers are lifted out
//! of the text, and OSC titles, progress, and OSC 133 [`ShellIntegration`] markers are retained for
//! agent and prompt detection. Every intercepted sequence is bounded and survives being split
//! across `feed` calls.
//!
//! The [`pty`] module starts processes attached to a platform pseudo-terminal.

#![cfg_attr(
    not(unix),
    allow(
        dead_code,
        reason = "some terminal helpers serve only the Unix PTY integration"
    )
)]

mod cell;
mod dcs;
mod event;
mod kitty;
mod marker;
mod osc;
pub mod pty;
mod terminal;

#[doc(inline)]
pub use cell::{Cell, TerminalColor, TerminalHyperlink, UnderlineStyle};
#[doc(inline)]
pub use event::{KittyGraphicsCommand, TerminalEvent, TerminalModes};
#[doc(inline)]
pub use osc::{ShellIntegration, ShellPhase};
#[doc(inline)]
pub use terminal::Terminal;
