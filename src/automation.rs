use std::env;
use std::io::{self, Write};
use std::time::Duration;

use base64::Engine;
use clap::{Args, Subcommand, ValueEnum};

use crate::ipc::{
    AutomationMethod, AutomationRequest, AutomationResponse, Axis, ClientMessage, ServerMessage,
};

const MAX_TIMEOUT_MS: u64 = 24 * 60 * 60 * 1000;
const MAX_INPUT_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum SplitAxis {
    Vertical,
    Horizontal,
}

impl From<SplitAxis> for Axis {
    fn from(axis: SplitAxis) -> Self {
        match axis {
            SplitAxis::Vertical => Self::Vertical,
            SplitAxis::Horizontal => Self::Horizontal,
        }
    }
}

#[derive(Debug, Args)]
pub struct PaneTarget {
    #[arg(long)]
    pane_id: Option<u64>,
}

#[derive(Debug, Subcommand)]
pub enum MsgCommand {
    /// Print the server's pane-automation capabilities.
    Capabilities,
    /// List every pane in deterministic pane-ID order.
    ListPanes,
    /// Inspect one pane.
    Inspect(PaneTarget),
    /// Inspect sanitized Vivid media state owned by one pane.
    InspectMedia(PaneTarget),
    /// Split a tiled pane without changing the active tab.
    Split {
        #[arg(value_enum)]
        axis: SplitAxis,
        #[arg(long)]
        pane_id: Option<u64>,
    },
    /// Activate and focus one pane.
    Focus(PaneTarget),
    /// Close one explicitly identified pane.
    ClosePane {
        #[arg(long)]
        pane_id: Option<u64>,
    },
    /// Write literal UTF-8 bytes to a pane's PTY.
    Typing {
        text: String,
        #[arg(long)]
        pane_id: Option<u64>,
    },
    /// Encode and write a terminal key.
    Key {
        key: String,
        #[arg(long, value_delimiter = ',')]
        mods: Vec<String>,
        #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u16).range(1..=1000))]
        repeat: u16,
        #[arg(long)]
        pane_id: Option<u64>,
    },
    /// Paste text, honoring the pane's bracketed-paste mode.
    Paste {
        text: String,
        #[arg(long)]
        pane_id: Option<u64>,
    },
    /// Print pane text exactly, without a trailing newline.
    GetText {
        #[arg(long, value_parser = clap::value_parser!(u16).range(1..=1000))]
        rows: Option<u16>,
        #[arg(long)]
        pane_id: Option<u64>,
    },
    /// Print a structured pane grid snapshot or viewport delta.
    GetGrid {
        #[arg(
            long,
            requires = "row_count",
            conflicts_with = "since_screen",
            allow_hyphen_values = true
        )]
        start_line: Option<isize>,
        #[arg(long, requires = "start_line", conflicts_with = "since_screen", value_parser = clap::value_parser!(u16).range(1..=1000))]
        row_count: Option<u16>,
        #[arg(long)]
        since_screen: Option<u64>,
        #[arg(long)]
        pane_id: Option<u64>,
    },
    /// Wait for pane or render state.
    Wait {
        #[command(subcommand)]
        command: WaitCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum WaitCommand {
    /// Wait until visible pane text contains a literal or regular expression.
    Text {
        text: String,
        #[arg(long)]
        regex: bool,
        #[arg(long)]
        after_screen: Option<u64>,
        #[arg(long, default_value = "30s", value_parser = parse_timeout)]
        timeout: Duration,
        #[arg(long)]
        pane_id: Option<u64>,
    },
    /// Wait for a newer pane screen sequence.
    ScreenChange {
        #[arg(long)]
        after_screen: Option<u64>,
        #[arg(long, default_value = "30s", value_parser = parse_timeout)]
        timeout: Duration,
        #[arg(long)]
        pane_id: Option<u64>,
    },
    /// Wait until a pane screen stays unchanged for a quiet period.
    ScreenStable {
        #[arg(long, default_value = "200ms", value_parser = parse_timeout)]
        quiet: Duration,
        #[arg(long)]
        after_screen: Option<u64>,
        #[arg(long, default_value = "30s", value_parser = parse_timeout)]
        timeout: Duration,
        #[arg(long)]
        pane_id: Option<u64>,
    },
    /// Wait for the attached client to acknowledge a composite render.
    Rendered {
        #[arg(long)]
        after_session: u64,
        #[arg(long, default_value = "30s", value_parser = parse_timeout)]
        timeout: Duration,
    },
    /// Wait for a pane process to exit.
    Exit {
        #[arg(long, default_value = "30s", value_parser = parse_timeout)]
        timeout: Duration,
        #[arg(long)]
        pane_id: Option<u64>,
    },
    /// Wait for a newer virtual or outer media projection revision.
    Media {
        #[arg(long)]
        after_virtual: Option<u64>,
        #[arg(long)]
        after_outer: Option<u64>,
        #[arg(long, default_value = "30s", value_parser = parse_timeout)]
        timeout: Duration,
        #[arg(long)]
        pane_id: Option<u64>,
    },
}

pub fn run(explicit_target: Option<&str>, command: MsgCommand) -> io::Result<()> {
    let target = explicit_target
        .map(ToOwned::to_owned)
        .or_else(|| {
            env::var("VVMUX_SESSION")
                .ok()
                .filter(|value| !value.is_empty())
        })
        .unwrap_or_else(|| "default".into());
    crate::runtime::validate_session_name(&target)?;
    let inherited_pane = (env::var("VVMUX_SESSION").ok().as_deref() == Some(target.as_str()))
        .then(inherited_pane_from_environment)
        .flatten();
    let (method, explicit_pane, allow_focused, output) = build_request(command)?;
    let pane_id = explicit_pane.or(inherited_pane);
    if matches!(&method, AutomationMethod::ClosePane) && pane_id.is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "close-pane requires --pane-id or a same-session VVMUX_PANE_ID",
        ));
    }
    let request = AutomationRequest {
        id: 1,
        pane_id,
        allow_focused,
        method,
    };
    let (mut reader, writer) = crate::server::connect(&target)?;
    writer
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .send(&ClientMessage::Automation(request))?;
    let response = receive_response(&mut reader, 1)?;
    if !response.ok {
        let error = response
            .error
            .map(|error| format!("{}: {}", error.code, error.message))
            .unwrap_or_else(|| "automation request failed".into());
        return Err(io::Error::other(error));
    }
    let result = response.result.unwrap_or(serde_json::Value::Null);
    match output {
        Output::Silent => Ok(()),
        Output::Text => {
            let text = result.as_str().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "get-text returned non-text data",
                )
            })?;
            io::stdout().write_all(text.as_bytes())
        }
        Output::Json => {
            serde_json::to_writer(io::stdout().lock(), &result).map_err(io::Error::other)?;
            println!();
            Ok(())
        }
    }
}

#[derive(Clone, Copy)]
enum Output {
    Silent,
    Text,
    Json,
}

fn build_request(command: MsgCommand) -> io::Result<(AutomationMethod, Option<u64>, bool, Output)> {
    let tuple = match command {
        MsgCommand::Capabilities => (AutomationMethod::Capabilities, None, false, Output::Json),
        MsgCommand::ListPanes => (AutomationMethod::ListPanes, None, false, Output::Json),
        MsgCommand::Inspect(target) => (
            AutomationMethod::Inspect,
            target.pane_id,
            true,
            Output::Json,
        ),
        MsgCommand::InspectMedia(target) => (
            AutomationMethod::InspectMedia,
            target.pane_id,
            true,
            Output::Json,
        ),
        MsgCommand::Split { axis, pane_id } => (
            AutomationMethod::Split { axis: axis.into() },
            pane_id,
            true,
            Output::Json,
        ),
        MsgCommand::Focus(target) => (
            AutomationMethod::Focus,
            target.pane_id,
            true,
            Output::Silent,
        ),
        MsgCommand::ClosePane { pane_id } => {
            (AutomationMethod::ClosePane, pane_id, false, Output::Silent)
        }
        MsgCommand::Typing { text, pane_id } => {
            validate_input(&text)?;
            (
                AutomationMethod::Typing { text },
                pane_id,
                true,
                Output::Silent,
            )
        }
        MsgCommand::Key {
            key,
            mods,
            repeat,
            pane_id,
        } => (
            AutomationMethod::Key {
                key,
                modifiers: mods,
                repeat,
            },
            pane_id,
            true,
            Output::Silent,
        ),
        MsgCommand::Paste { text, pane_id } => {
            validate_input(&text)?;
            (
                AutomationMethod::Paste { text },
                pane_id,
                true,
                Output::Silent,
            )
        }
        MsgCommand::GetText { rows, pane_id } => (
            AutomationMethod::GetText { rows },
            pane_id,
            true,
            Output::Text,
        ),
        MsgCommand::GetGrid {
            start_line,
            row_count,
            since_screen,
            pane_id,
        } => (
            AutomationMethod::GetGrid {
                start_line,
                row_count,
                since_screen,
            },
            pane_id,
            true,
            Output::Json,
        ),
        MsgCommand::Wait { command } => match command {
            WaitCommand::Text {
                text,
                regex,
                after_screen,
                timeout,
                pane_id,
            } => {
                if regex && text.len() > 8 * 1024 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "regular expression exceeds 8 KiB",
                    ));
                }
                (
                    AutomationMethod::WaitText {
                        text,
                        regex,
                        after_screen,
                        timeout_ms: millis(timeout),
                    },
                    pane_id,
                    true,
                    Output::Json,
                )
            }
            WaitCommand::ScreenChange {
                after_screen,
                timeout,
                pane_id,
            } => (
                AutomationMethod::WaitScreenChange {
                    after_screen,
                    timeout_ms: millis(timeout),
                },
                pane_id,
                true,
                Output::Json,
            ),
            WaitCommand::ScreenStable {
                quiet,
                after_screen,
                timeout,
                pane_id,
            } => (
                AutomationMethod::WaitScreenStable {
                    quiet_ms: millis(quiet),
                    after_screen,
                    timeout_ms: millis(timeout),
                },
                pane_id,
                true,
                Output::Json,
            ),
            WaitCommand::Rendered {
                after_session,
                timeout,
            } => (
                AutomationMethod::WaitRendered {
                    after_session,
                    timeout_ms: millis(timeout),
                },
                None,
                false,
                Output::Json,
            ),
            WaitCommand::Exit { timeout, pane_id } => (
                AutomationMethod::WaitExit {
                    timeout_ms: millis(timeout),
                },
                pane_id,
                true,
                Output::Json,
            ),
            WaitCommand::Media {
                after_virtual,
                after_outer,
                timeout,
                pane_id,
            } => (
                AutomationMethod::WaitMedia {
                    after_virtual_revision: after_virtual,
                    after_outer_revision: after_outer,
                    timeout_ms: millis(timeout),
                },
                pane_id,
                true,
                Output::Json,
            ),
        },
    };
    Ok(tuple)
}

fn receive_response(
    reader: &mut crate::ipc::RecordReader,
    id: u64,
) -> io::Result<AutomationResponse> {
    let mut chunks = Vec::new();
    let mut next_index = 0;
    loop {
        match reader.recv::<ServerMessage>()? {
            ServerMessage::Automation(response) if response.id == id => return Ok(response),
            ServerMessage::AutomationChunk {
                request_id,
                index,
                last,
                base64,
            } if request_id == id => {
                if index != next_index {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "automation response chunk sequence gap",
                    ));
                }
                let decoded = base64::engine::general_purpose::STANDARD
                    .decode(base64)
                    .map_err(|_| {
                        io::Error::new(io::ErrorKind::InvalidData, "invalid response base64")
                    })?;
                if chunks.len().saturating_add(decoded.len()) > 16 * 1024 * 1024 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "automation response exceeds 16 MiB",
                    ));
                }
                chunks.extend_from_slice(&decoded);
                next_index += 1;
                if last {
                    return serde_json::from_slice(&chunks).map_err(|error| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("invalid automation response: {error}"),
                        )
                    });
                }
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "unexpected VVMX message on automation connection",
                ));
            }
        }
    }
}

fn validate_input(text: &str) -> io::Result<()> {
    if text.len() > MAX_INPUT_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "input exceeds 1 MiB",
        ));
    }
    Ok(())
}

fn inherited_pane_from_environment() -> Option<u64> {
    env::var("VVMUX_PANE_ID").ok()?.parse().ok()
}

fn millis(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

fn parse_timeout(value: &str) -> Result<Duration, String> {
    let (number, multiplier) = if let Some(value) = value.strip_suffix("ms") {
        (value, 1_u64)
    } else if let Some(value) = value.strip_suffix('s') {
        (value, 1_000)
    } else if let Some(value) = value.strip_suffix('m') {
        (value, 60_000)
    } else if let Some(value) = value.strip_suffix('h') {
        (value, 3_600_000)
    } else {
        (value, 1)
    };
    let number = number
        .parse::<u64>()
        .map_err(|_| format!("invalid duration {value:?}"))?;
    let millis = number
        .checked_mul(multiplier)
        .ok_or_else(|| "duration is too large".to_string())?;
    if !(1..=MAX_TIMEOUT_MS).contains(&millis) {
        return Err("duration must be from 1ms through 24h".into());
    }
    Ok(Duration::from_millis(millis))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_bounds_and_units() {
        assert_eq!(parse_timeout("1ms").unwrap(), Duration::from_millis(1));
        assert_eq!(parse_timeout("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_timeout("24h").unwrap(), Duration::from_secs(86_400));
        assert!(parse_timeout("0").is_err());
        assert!(parse_timeout("25h").is_err());
    }
}
