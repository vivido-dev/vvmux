use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub general: General,
    pub appearance: Appearance,
    pub media: Media,
    pub keys: Keys,
    pub floating: Floating,
    pub server: Server,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct General {
    pub prefix: String,
    pub shell: Option<PathBuf>,
    pub default_cwd: Option<PathBuf>,
    pub scrollback_lines: usize,
    pub status_visible: bool,
    pub mouse: bool,
    pub render_interval_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Appearance {
    pub active_frame: u8,
    pub inactive_frame: u8,
    pub status_foreground: u8,
    pub status_background: u8,
}

pub use vivid_gateway::MediaConfig as Media;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Keys {
    pub prefix: BTreeMap<String, String>,
    pub copy: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Floating {
    pub default_width_percent: u16,
    pub default_height_percent: u16,
    pub border_drag_margin: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Server {
    pub listen: String,
    pub allowed_origins: Vec<String>,
    pub auth_file: Option<PathBuf>,
    pub max_connections: usize,
    pub outbound_queue_bytes: usize,
}

impl Default for Server {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:7880".into(),
            allowed_origins: Vec::new(),
            auth_file: None,
            max_connections: 32,
            outbound_queue_bytes: 4 * 1024 * 1024,
        }
    }
}

impl Default for Floating {
    fn default() -> Self {
        Self {
            default_width_percent: 60,
            default_height_percent: 60,
            border_drag_margin: 1,
        }
    }
}

impl Default for General {
    fn default() -> Self {
        Self {
            prefix: "C-b".into(),
            shell: None,
            default_cwd: None,
            scrollback_lines: 10_000,
            status_visible: true,
            mouse: true,
            render_interval_ms: 16,
        }
    }
}

impl Default for Appearance {
    fn default() -> Self {
        Self {
            active_frame: 12,
            inactive_frame: 8,
            status_foreground: 15,
            status_background: 4,
        }
    }
}

impl Config {
    pub fn load(override_path: Option<&Path>) -> io::Result<Self> {
        let path = override_path.map(Path::to_path_buf).or_else(default_path);
        let Some(path) = path else {
            return Ok(Self::default());
        };
        let source = match fs::read_to_string(&path) {
            Ok(source) => source,
            Err(error) if error.kind() == io::ErrorKind::NotFound && override_path.is_none() => {
                return Ok(Self::default());
            }
            Err(error) => return Err(error),
        };
        let config: Self = toml::from_str(&source).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid vvmux config {}: {error}", path.display()),
            )
        })?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> io::Result<()> {
        if parse_control_chord(&self.general.prefix).is_none() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "prefix must be a control chord such as \"C-b\"",
            ));
        }
        if self.general.scrollback_lines > 1_000_000 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "scrollback_lines exceeds 1,000,000",
            ));
        }
        if !(1..=1000).contains(&self.general.render_interval_ms) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "render_interval_ms must be between 1 and 1000",
            ));
        }
        if self.media.aggregate_retained_bytes < 64 * 1024 * 1024 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "aggregate_retained_bytes must admit one maximum Vivid record",
            ));
        }
        if !(10..=100).contains(&self.floating.default_width_percent)
            || !(10..=100).contains(&self.floating.default_height_percent)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "[floating] default size percentages must be between 10 and 100",
            ));
        }
        if !(1..=4).contains(&self.floating.border_drag_margin) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "[floating] border_drag_margin must be between 1 and 4",
            ));
        }
        let listen = self
            .server
            .listen
            .parse::<std::net::SocketAddr>()
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "[server].listen must be an IP socket address",
                )
            })?;
        if !listen.ip().is_loopback() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "[server].listen must use a loopback address",
            ));
        }
        if !(1..=1024).contains(&self.server.max_connections) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "[server].max_connections must be between 1 and 1024",
            ));
        }
        if !(512 * 1024..=64 * 1024 * 1024).contains(&self.server.outbound_queue_bytes) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "[server].outbound_queue_bytes must be between 512 KiB and 64 MiB",
            ));
        }
        const PREFIX_ACTIONS: &[&str] = &[
            "split-horizontal",
            "split-vertical",
            "focus-left",
            "focus-right",
            "focus-up",
            "focus-down",
            "resize-left",
            "resize-right",
            "resize-up",
            "resize-down",
            "new-tab",
            "next-tab",
            "previous-tab",
            "close-pane",
            "toggle-zoom",
            "copy-mode",
            "paste",
            "new-floating-pane",
            "toggle-floating-panes",
            "toggle-pane-pinned",
            "enter-floating-move-mode",
            "enter-floating-resize-mode",
        ];
        if self.keys.prefix.iter().any(|(chord, action)| {
            chord.chars().count() != 1 || !PREFIX_ACTIONS.contains(&action.as_str())
        }) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "[keys.prefix] contains an unsupported chord or action",
            ));
        }
        const COPY_CHORDS: &[&str] = &[
            "Up", "Down", "Left", "Right", "PageUp", "PageDown", "Space", "Enter", "q", "Escape",
        ];
        const COPY_ACTIONS: &[&str] = &[
            "up",
            "down",
            "left",
            "right",
            "page-up",
            "page-down",
            "start-selection",
            "copy",
            "cancel",
        ];
        if self.keys.copy.iter().any(|(chord, action)| {
            !COPY_CHORDS.contains(&chord.as_str()) || !COPY_ACTIONS.contains(&action.as_str())
        }) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "[keys.copy] contains an unsupported chord or action",
            ));
        }
        Ok(())
    }
}

pub fn parse_control_chord(chord: &str) -> Option<u8> {
    let bytes = chord.as_bytes();
    (bytes.len() == 3 && bytes[0] == b'C' && bytes[1] == b'-')
        .then(|| bytes[2].to_ascii_lowercase())
        .filter(|byte| byte.is_ascii_lowercase())
        .map(|byte| byte - b'a' + 1)
}

#[cfg(unix)]
fn default_path() -> Option<PathBuf> {
    if let Some(root) = std::env::var_os("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(root).join("vvmux/config.toml"));
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(".config/vvmux/config.toml"))
}

#[cfg(windows)]
fn default_path() -> Option<PathBuf> {
    crate::platform::windows_config_path()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_fields_are_rejected() {
        assert!(toml::from_str::<Config>("[general]\nunknown = true").is_err());
    }

    #[test]
    fn defaults_are_bounded() {
        let config = Config::default();
        config.validate().unwrap();
        assert_eq!(config.media.aggregate_retained_bytes, 256 * 1024 * 1024);
        assert_eq!(config.server.listen, "127.0.0.1:7880");
    }

    #[test]
    fn floating_bounds_and_actions_are_validated() {
        let mut config = Config::default();
        assert_eq!(config.floating.default_width_percent, 60);
        config.validate().unwrap();

        config.floating.default_width_percent = 9;
        assert!(config.validate().is_err());
        config.floating.default_width_percent = 100;
        config.validate().unwrap();
        config.floating.border_drag_margin = 5;
        assert!(config.validate().is_err());
        config.floating.border_drag_margin = 4;
        config.validate().unwrap();

        config
            .keys
            .prefix
            .insert("g".into(), "new-floating-pane".into());
        config.validate().unwrap();
        assert!(toml::from_str::<Config>("[floating]\nunknown = 1").is_err());
    }

    #[test]
    fn control_prefixes_and_key_actions_are_validated() {
        assert_eq!(parse_control_chord("C-a"), Some(1));
        assert_eq!(parse_control_chord("C-z"), Some(26));
        assert_eq!(parse_control_chord("Alt-b"), None);

        let mut config = Config::default();
        config
            .keys
            .prefix
            .insert("f".into(), "not-an-action".into());
        assert!(config.validate().is_err());
    }

    #[test]
    fn network_server_is_loopback_and_bounded() {
        let mut config = Config::default();
        config.server.listen = "0.0.0.0:7880".into();
        assert!(config.validate().is_err());
        config.server.listen = "[::1]:7880".into();
        config.validate().unwrap();
        config.server.outbound_queue_bytes = 511 * 1024;
        assert!(config.validate().is_err());
        config.server.outbound_queue_bytes = 64 * 1024 * 1024 + 1;
        assert!(config.validate().is_err());
    }
}
