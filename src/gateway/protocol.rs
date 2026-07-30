use std::io;

use serde::{Deserialize, Serialize};

use crate::ipc::{Action, Axis, Direction, DisplayMetrics};

pub(crate) const VERSION: u16 = 1;
pub(crate) const SUBPROTOCOL: &str = "vvmux.v1";
pub(crate) const MAX_CONTROL_BYTES: usize = 64 * 1024;
pub(crate) const MAX_INPUT_BYTES: usize = 64 * 1024;
pub(crate) const MAX_FRAME_BYTES: usize = 1024 * 1024;

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum ClientControl {
    Hello {
        protocol: u16,
        token: String,
    },
    ListSessions {
        request_id: u64,
    },
    CreateSession {
        request_id: u64,
        name: String,
    },
    Attach {
        request_id: u64,
        name: String,
        display: WireDisplay,
        #[serde(default)]
        takeover: bool,
        #[serde(default)]
        vivid: bool,
    },
    Resize {
        display: WireDisplay,
    },
    Action {
        action: WireAction,
    },
    Detach,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct WireDisplay {
    pub columns: u16,
    pub rows: u16,
    #[serde(default)]
    pub cell_width: u16,
    #[serde(default)]
    pub cell_height: u16,
}

impl WireDisplay {
    pub(crate) fn validate(self) -> io::Result<DisplayMetrics> {
        if !(10..=1000).contains(&self.columns)
            || !(4..=500).contains(&self.rows)
            || self.cell_width > 4096
            || self.cell_height > 4096
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "terminal display metrics are outside VVWS/1 limits",
            ));
        }
        Ok(DisplayMetrics {
            columns: self.columns,
            rows: self.rows,
            cell_width: self.cell_width,
            cell_height: self.cell_height,
        })
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(tag = "name", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum WireAction {
    Split { axis: WireAxis },
    Focus { direction: WireDirection },
    Resize { direction: WireDirection },
    NewTab,
    NextTab,
    PreviousTab,
    SelectTab { index: u16 },
    ClosePane,
    ToggleZoom,
    EnterCopyMode,
    CopyInput { bytes: Vec<u8> },
    Paste,
    NewFloatingPane,
    ToggleFloatingPanes,
    TogglePanePinned,
    EnterFloatingMoveMode,
    EnterFloatingResizeMode,
}

impl WireAction {
    pub(crate) fn into_ipc(self) -> io::Result<Action> {
        match self {
            Self::Split { axis } => Ok(Action::Split(axis.into())),
            Self::Focus { direction } => Ok(Action::Focus(direction.into())),
            Self::Resize { direction } => Ok(Action::Resize(direction.into())),
            Self::NewTab => Ok(Action::NewTab),
            Self::NextTab => Ok(Action::NextTab),
            Self::PreviousTab => Ok(Action::PreviousTab),
            Self::SelectTab { index } => Ok(Action::SelectTab(index as usize)),
            Self::ClosePane => Ok(Action::ClosePane),
            Self::ToggleZoom => Ok(Action::ToggleZoom),
            Self::EnterCopyMode => Ok(Action::EnterCopyMode),
            Self::CopyInput { bytes } if bytes.len() <= MAX_INPUT_BYTES => {
                Ok(Action::CopyInput(bytes))
            }
            Self::CopyInput { .. } => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "copy input exceeds 64 KiB",
            )),
            Self::Paste => Ok(Action::Paste),
            Self::NewFloatingPane => Ok(Action::NewFloatingPane),
            Self::ToggleFloatingPanes => Ok(Action::ToggleFloatingPanes),
            Self::TogglePanePinned => Ok(Action::TogglePanePinned),
            Self::EnterFloatingMoveMode => Ok(Action::EnterFloatingMoveMode),
            Self::EnterFloatingResizeMode => Ok(Action::EnterFloatingResizeMode),
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WireAxis {
    Horizontal,
    Vertical,
}

impl From<WireAxis> for Axis {
    fn from(value: WireAxis) -> Self {
        match value {
            WireAxis::Horizontal => Self::Horizontal,
            WireAxis::Vertical => Self::Vertical,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WireDirection {
    Left,
    Right,
    Up,
    Down,
}

impl From<WireDirection> for Direction {
    fn from(value: WireDirection) -> Self {
        match value {
            WireDirection::Left => Self::Left,
            WireDirection::Right => Self::Right,
            WireDirection::Up => Self::Up,
            WireDirection::Down => Self::Down,
        }
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct VividAccess<'a> {
    pub endpoint: &'a str,
    pub subprotocol: &'static str,
    /// Vivid wire protocol carried by this otherwise byte-transparent route.
    pub wire_version: &'static str,
    pub connection: &'a str,
    pub token: &'a str,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ServerControl<'a> {
    Hello {
        protocol: u16,
        server_version: &'static str,
        capabilities: &'static [&'static str],
        vivid: VividAccess<'a>,
    },
    Sessions {
        request_id: u64,
        sessions: Vec<SessionInfo>,
    },
    Created {
        request_id: u64,
        name: String,
    },
    Attached {
        request_id: u64,
        name: String,
        text_only: bool,
    },
    Title {
        title: String,
    },
    Bell,
    Clipboard {
        text: String,
    },
    Status {
        message: String,
    },
    FloatingEditState {
        mode_id: u64,
        active: bool,
        pane: Option<u64>,
        kind: Option<&'static str>,
    },
    Detached {
        reason: String,
    },
    Error {
        request_id: Option<u64>,
        code: &'static str,
        message: String,
    },
}

#[derive(Debug, Serialize)]
pub(crate) struct SessionInfo {
    pub name: String,
    pub pid: u32,
}

pub(crate) fn decode_control(text: &str) -> io::Result<ClientControl> {
    if text.len() > MAX_CONTROL_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "VVWS/1 control message exceeds 64 KiB",
        ));
    }
    serde_json::from_str(text).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("malformed VVWS/1 control message: {error}"),
        )
    })
}

pub(crate) fn encode_control(message: &ServerControl<'_>) -> io::Result<String> {
    let encoded = serde_json::to_string(message).map_err(io::Error::other)?;
    if encoded.len() > MAX_CONTROL_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "VVWS/1 control message exceeds 64 KiB",
        ));
    }
    Ok(encoded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_controls_and_display_limits() {
        assert_eq!(
            decode_control(r#"{"type":"list_sessions","request_id":7}"#).unwrap(),
            ClientControl::ListSessions { request_id: 7 }
        );
        assert!(decode_control(r#"{"type":"list_sessions","request_id":7,"extra":true}"#).is_err());
        assert!(
            WireDisplay {
                columns: 9,
                rows: 24,
                cell_width: 8,
                cell_height: 16,
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn every_wire_action_maps_to_existing_ipc() {
        assert_eq!(
            WireAction::Split {
                axis: WireAxis::Vertical,
            }
            .into_ipc()
            .unwrap(),
            Action::Split(Axis::Vertical)
        );
        assert_eq!(
            WireAction::Focus {
                direction: WireDirection::Left,
            }
            .into_ipc()
            .unwrap(),
            Action::Focus(Direction::Left)
        );
        assert!(
            WireAction::CopyInput {
                bytes: vec![0; MAX_INPUT_BYTES + 1],
            }
            .into_ipc()
            .is_err()
        );
    }

    #[test]
    fn outbound_controls_are_limited_after_json_escaping() {
        assert!(
            encode_control(&ServerControl::Clipboard {
                text: "\\".repeat(MAX_CONTROL_BYTES),
            })
            .is_err()
        );
    }
}
