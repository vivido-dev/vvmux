use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub general: General,
    /// Deprecated in favor of `[theme]`, which supersedes it and adds truecolor. Still honored:
    /// see `crate::theme::resolve`.
    pub appearance: Appearance,
    pub theme: Theme,
    pub media: Media,
    pub keys: Keys,
    pub floating: Floating,
    pub plugins: Plugins,
    pub server: Server,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct General {
    pub prefix: String,
    pub shell: Option<PathBuf>,
    pub default_cwd: Option<PathBuf>,
    pub default_layout: Option<String>,
    pub scrollback_lines: usize,
    pub status_visible: bool,
    pub mouse: bool,
    pub render_interval_ms: u64,
}

/// The deprecated color section, superseded by `[theme]`.
///
/// Every field is optional so resolution can tell "the user chose this" from "nothing was
/// written": a `[theme].preset` has to be able to win over a key nobody set, and a plain `u8`
/// with a default cannot express that. The pre-theme defaults now live in `crate::theme`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Appearance {
    pub active_frame: Option<u8>,
    pub inactive_frame: Option<u8>,
    pub status_foreground: Option<u8>,
    pub status_background: Option<u8>,
}

pub use crate::theme::Theme;
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Plugins {
    /// Global live-session kill switch. Individual packages retain their registry enable state.
    pub enabled: bool,
}

impl Default for Plugins {
    fn default() -> Self {
        Self { enabled: true }
    }
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
            default_layout: None,
            scrollback_lines: 10_000,
            status_visible: true,
            mouse: true,
            render_interval_ms: 16,
        }
    }
}

impl Config {
    pub fn load(override_path: Option<&Path>) -> io::Result<Self> {
        Self::load_with_path(override_path).map(|(config, _)| config)
    }

    /// Load the config and report which file it came from.
    ///
    /// The session server needs the resolved path so it can watch that file for changes; `load`
    /// discards it. The path is `None` only when no config file could be resolved at all.
    pub fn load_with_path(override_path: Option<&Path>) -> io::Result<(Self, Option<PathBuf>)> {
        let path = override_path.map(Path::to_path_buf).or_else(default_path);
        let Some(path) = path else {
            return Ok((Self::default(), None));
        };
        let source = match fs::read_to_string(&path) {
            Ok(source) => source,
            Err(error) if error.kind() == io::ErrorKind::NotFound && override_path.is_none() => {
                // A missing default config is not an error, but the path still matters: a file
                // created later must be picked up by a reload.
                return Ok((Self::default(), Some(path)));
            }
            Err(error) => return Err(error),
        };
        let config = Self::parse(&source, &path)?;
        Ok((config, Some(path)))
    }

    /// Parse and validate one config source, naming `path` in any error.
    pub fn parse(source: &str, path: &Path) -> io::Result<Self> {
        let config: Self = toml::from_str(source).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "invalid vvmux config {}: {}",
                    path.display(),
                    single_line_toml_error(&error, source)
                ),
            )
        })?;
        config.validate()?;
        Ok(config)
    }

    /// The effective colors, after `[theme]`, `[appearance]`, the preset, and the built-in
    /// default have been layered. Cheap enough to call once per render.
    pub fn resolved_theme(&self) -> crate::theme::ResolvedTheme {
        crate::theme::resolve(&self.theme, &self.appearance)
    }

    pub fn validate(&self) -> io::Result<()> {
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
        if let Some(layout) = &self.general.default_layout
            && !crate::layout_file::validate_default_name(layout)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "default_layout must be non-empty and must not contain '..'",
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
        if let Some(preset) = &self.theme.preset
            && crate::theme::preset(preset).is_none()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "[theme].preset \"{preset}\" is not a known preset; valid names are {}",
                    crate::theme::PRESET_NAMES.join(", ")
                ),
            ));
        }
        // The action vocabularies are defined by the code that interprets them, not duplicated
        // here: `parse_configured_action` is what the client actually dispatches on, and
        // `copy_action_bytes` is what copy mode actually replays. Validating through them means a
        // new action cannot be accepted by config but unhandled at runtime, or vice versa.
        if self.keys.prefix.iter().any(|(chord, action)| {
            chord.chars().count() != 1
                || crate::client_input::parse_configured_action(action).is_none()
        }) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "[keys.prefix] contains an unsupported chord or action",
            ));
        }
        const COPY_CHORDS: &[&str] = &[
            "Up", "Down", "Left", "Right", "PageUp", "PageDown", "Space", "Enter", "q", "Escape",
            "/", "?", "n", "N",
        ];
        if self.keys.copy.iter().any(|(chord, action)| {
            !COPY_CHORDS.contains(&chord.as_str())
                || crate::session::copy_action_bytes(action).is_none()
        }) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "[keys.copy] contains an unsupported chord or action",
            ));
        }
        Ok(())
    }
}

/// Collapse a `toml` parse error onto one line, keeping where it happened.
///
/// The crate's `Display` is a multi-line snippet: a header with the position, a caret diagram, and
/// the reason last. A config failure is reported on the single-line status bar, which showed only
/// the tail — so `[theme]:` surfaced as "unexpected key or value, expected newline, `#`" and read
/// like the file's `#` comments had been rejected, when the stray colon was the whole problem.
/// Keeping the position and the offending text with the reason makes the real line obvious.
fn single_line_toml_error(error: &toml::de::Error, source: &str) -> String {
    let message = error.message().replace('\n', " ");
    let Some(start) = error
        .span()
        .map(|span| span.start)
        .filter(|start| source.is_char_boundary(*start))
    else {
        return message;
    };
    let line_start = source[..start].rfind('\n').map_or(0, |newline| newline + 1);
    let line_end = source[line_start..]
        .find('\n')
        .map_or(source.len(), |offset| line_start + offset);
    let line = source[line_start..line_end].trim();
    let mut snippet: String = line.chars().take(SNIPPET_CHARS).collect();
    if line.chars().count() > SNIPPET_CHARS {
        snippet.push('…');
    }
    format!(
        "line {}, column {}: {message}{}",
        source[..start].matches('\n').count() + 1,
        source[line_start..start].chars().count() + 1,
        if snippet.is_empty() {
            String::new()
        } else {
            format!(" (in `{snippet}`)")
        }
    )
}

/// How much of the offending line the status bar quotes back. Long enough to recognize the line,
/// short enough that the reason still fits beside it.
const SNIPPET_CHARS: usize = 40;

pub fn parse_control_chord(chord: &str) -> Option<u8> {
    let bytes = chord.as_bytes();
    (bytes.len() == 3 && bytes[0] == b'C' && bytes[1] == b'-')
        .then(|| bytes[2].to_ascii_lowercase())
        .filter(|byte| byte.is_ascii_lowercase())
        .map(|byte| byte - b'a' + 1)
}

#[cfg(unix)]
pub fn default_path() -> Option<PathBuf> {
    if let Some(root) = std::env::var_os("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(root).join("vvmux/config.toml"));
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(".config/vvmux/config.toml"))
}

pub fn config_dir() -> Option<PathBuf> {
    default_path()?.parent().map(Path::to_path_buf)
}

#[cfg(windows)]
pub fn default_path() -> Option<PathBuf> {
    crate::platform::windows_config_path()
}

#[cfg(test)]
mod tests {
    use super::*;
    use vvmux_terminal::TerminalColor;

    #[test]
    fn unknown_fields_are_rejected() {
        assert!(toml::from_str::<Config>("[general]\nunknown = true").is_err());
    }

    /// Comments are ordinary TOML and must survive everywhere a user writes them: above a table,
    /// above a key, and trailing one.
    #[test]
    fn comments_are_accepted_anywhere() {
        let config = Config::parse(
            "# vvmux config\n\
             [theme]\n\
             # default, mono, nord, solarized-dark, gruvbox-dark\n\
             preset = \"nord\" # trailing\n\
             \n\
             [general] # trailing on a header\n\
             mouse = false\n",
            Path::new("config.toml"),
        )
        .expect("comments must not be a parse error");
        assert_eq!(config.theme.preset.as_deref(), Some("nord"));
        assert!(!config.general.mouse);
    }

    /// A syntax error is reported on the one-line status bar. The `toml` crate's own `Display` is
    /// a multi-line caret diagram whose *last* line is the reason, so the bar showed only
    /// "unexpected key or value, expected newline, `#`" for a stray `[theme]:` — which reads like
    /// the comments were rejected. The position and the offending line have to survive.
    #[test]
    fn a_syntax_error_names_its_line_on_one_line() {
        let error = Config::parse(
            "[theme]:\n# default, mono, nord\npreset = \"nord\"\n",
            Path::new("/tmp/config.toml"),
        )
        .expect_err("a stray colon must not parse");
        let message = error.to_string();
        assert!(
            !message.contains('\n'),
            "the status bar shows one line: {message}"
        );
        assert!(
            message.contains("line 1, column 8") && message.contains("in `[theme]:`"),
            "the error must point at the colon, not the comment: {message}"
        );
        assert!(
            message.contains("/tmp/config.toml"),
            "the failing file is still named: {message}"
        );
    }

    /// A key error is reported against the key's own line, not the start of the file.
    #[test]
    fn an_unknown_key_is_located_after_a_comment_block() {
        let error = Config::parse(
            "# one\n# two\n[general]\nunknown = true\n",
            Path::new("config.toml"),
        )
        .expect_err("an unknown key must not parse");
        let message = error.to_string();
        assert!(message.contains("line 4"), "{message}");
        assert!(message.contains("unknown field `unknown`"), "{message}");
    }

    #[test]
    fn defaults_are_bounded() {
        let config = Config::default();
        config.validate().unwrap();
        assert_eq!(config.media.aggregate_retained_bytes, 256 * 1024 * 1024);
        assert!(config.plugins.enabled);
        assert_eq!(config.server.listen, "127.0.0.1:7880");
    }

    #[test]
    fn plugin_kill_switch_is_strict_and_can_be_disabled() {
        let config: Config = toml::from_str("[plugins]\nenabled = false").unwrap();
        assert!(!config.plugins.enabled);
        assert!(toml::from_str::<Config>("[plugins]\nunknown = true").is_err());
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

    /// The documented vocabulary and the dispatch tables must agree in both directions.
    ///
    /// Validation delegates to `parse_configured_action` / `copy_action_bytes`, so this test is
    /// what keeps the README and `config.example.toml` honest: a renamed action breaks it here
    /// rather than silently becoming unbindable.
    #[test]
    fn every_documented_action_name_is_bindable() {
        const DOCUMENTED_PREFIX_ACTIONS: &[&str] = &[
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
            "tab-navigator",
            "rename-tab",
            "confirm-close-pane",
            "close-pane",
            "toggle-zoom",
            "toggle-sync-input",
            "copy-mode",
            "paste",
            "new-floating-pane",
            "toggle-floating-panes",
            "toggle-pane-pinned",
            "enter-floating-move-mode",
            "enter-floating-resize-mode",
            "agent-navigator",
            "save-layout",
        ];
        const DOCUMENTED_COPY_ACTIONS: &[&str] = &[
            "up",
            "down",
            "left",
            "right",
            "page-up",
            "page-down",
            "start-selection",
            "copy",
            "cancel",
            "search-forward",
            "search-backward",
            "search-next",
            "search-previous",
        ];

        for action in DOCUMENTED_PREFIX_ACTIONS {
            assert!(
                crate::client_input::parse_configured_action(action).is_some(),
                "documented prefix action {action} is not dispatchable"
            );
            let mut config = Config::default();
            config.keys.prefix.insert("g".into(), (*action).into());
            config
                .validate()
                .unwrap_or_else(|error| panic!("{action} rejected by validation: {error}"));
        }

        for action in DOCUMENTED_COPY_ACTIONS {
            assert!(
                crate::session::copy_action_bytes(action).is_some(),
                "documented copy action {action} has no key bytes"
            );
            let mut config = Config::default();
            config.keys.copy.insert("q".into(), (*action).into());
            config
                .validate()
                .unwrap_or_else(|error| panic!("{action} rejected by validation: {error}"));
        }

        let mut unknown_copy = Config::default();
        unknown_copy.keys.copy.insert("q".into(), "warp".into());
        assert!(unknown_copy.validate().is_err());

        let mut unknown_chord = Config::default();
        unknown_chord.keys.copy.insert("F1".into(), "up".into());
        assert!(unknown_chord.validate().is_err());
    }

    #[test]
    fn a_theme_section_parses_every_color_form() {
        // `r##"…"##`: the hex colors contain `"#`, which would close an `r#"…"#` string.
        let config: Config = toml::from_str(
            r##"
[theme]
preset = "nord"
active_frame = "#ff8800"
inactive_frame = "bright-black"
status_foreground = "15"
status_background = "default"
status_fill = false
"##,
        )
        .unwrap();
        config.validate().unwrap();

        let resolved = config.resolved_theme();
        assert_eq!(resolved.active_frame, TerminalColor::Rgb(0xff, 0x88, 0x00));
        assert_eq!(resolved.inactive_frame, TerminalColor::Indexed(8));
        assert_eq!(resolved.status_foreground, TerminalColor::Indexed(15));
        assert_eq!(resolved.status_background, TerminalColor::Default);
        assert!(!resolved.status_fill);
        assert_eq!(
            resolved.active_title,
            crate::theme::preset("nord").unwrap().active_title,
            "keys the user did not set still come from the preset"
        );
    }

    #[test]
    fn a_malformed_theme_color_names_the_accepted_forms() {
        let error = toml::from_str::<Config>("[theme]\nactive_frame = \"chartreuse\"")
            .expect_err("an unknown color must not parse");
        let described = error.to_string();
        assert!(described.contains("#rrggbb"), "{described}");

        assert!(
            toml::from_str::<Config>("[theme]\nactive_frame = 12").is_err(),
            "a bare integer is not the documented form; colors are strings"
        );
        assert!(toml::from_str::<Config>("[theme]\nunknown = \"red\"").is_err());
    }

    #[test]
    fn an_unknown_preset_is_rejected_with_the_valid_names() {
        let config: Config = toml::from_str("[theme]\npreset = \"dracula\"").unwrap();
        let error = config.validate().expect_err("unknown preset must fail");
        let described = error.to_string();

        assert!(described.contains("dracula"), "{described}");
        assert!(described.contains("solarized-dark"), "{described}");
    }

    /// The pre-theme config must render exactly as it did before this section existed.
    #[test]
    fn a_config_without_a_theme_keeps_its_previous_colors() {
        let resolved = Config::default().resolved_theme();
        assert_eq!(resolved.active_frame, TerminalColor::Indexed(12));
        assert_eq!(resolved.inactive_frame, TerminalColor::Indexed(8));
        assert_eq!(resolved.status_foreground, TerminalColor::Indexed(15));
        assert_eq!(resolved.status_background, TerminalColor::Indexed(4));

        let legacy: Config = toml::from_str("[appearance]\nactive_frame = 2").unwrap();
        legacy.validate().unwrap();
        assert_eq!(
            legacy.resolved_theme().active_frame,
            TerminalColor::Indexed(2),
            "the deprecated section is still honored"
        );
        assert_eq!(
            legacy.resolved_theme().status_background,
            TerminalColor::Indexed(4),
            "and its unset keys keep the built-in default"
        );
    }

    /// `config.example.toml` is part of the documented interface; `deny_unknown_fields` means a
    /// stale example is a hard parse error for anyone who copies it.
    #[test]
    fn the_example_config_parses_and_validates() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("config.example.toml");
        let source = fs::read_to_string(&path).unwrap();
        Config::parse(&source, &path).expect("config.example.toml must stay loadable");
    }

    #[test]
    fn default_layout_names_are_bounded_paths() {
        let mut config = Config::default();
        config.general.default_layout = Some("dev".into());
        config.validate().unwrap();
        config.general.default_layout = Some("layouts/dev.toml".into());
        config.validate().unwrap();
        config.general.default_layout = Some("../secret".into());
        assert!(config.validate().is_err());
        config.general.default_layout = Some("  ".into());
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
