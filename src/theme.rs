//! Frame and status-bar colors.
//!
//! A color is written as a string so one key accepts every form the outer terminal can render:
//! `"default"`, an index `"0"`–`"255"`, an ANSI name such as `"bright-blue"`, or `"#rrggbb"`.
//! `ScreenBuffer`'s ANSI writer already encodes all three, so nothing here is renderer-specific.
//!
//! Resolution has four layers, most specific first: an explicit `[theme]` key, the deprecated
//! `[appearance]` key it replaced, the named `[theme].preset`, and finally the built-in default.
//! The middle layer is why `Appearance`'s fields are `Option` — a preset has to be able to win
//! over a field the user never wrote, which a plain `u8` default cannot express.

use serde::de::{Error as _, Unexpected};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use vvmux_terminal::TerminalColor;

use crate::config::Appearance;
use crate::screen::{FrameStyle, TextStyle};

/// The 16 ANSI color names, in palette order.
const ANSI_NAMES: [&str; 16] = [
    "black",
    "red",
    "green",
    "yellow",
    "blue",
    "magenta",
    "cyan",
    "white",
    "bright-black",
    "bright-red",
    "bright-green",
    "bright-yellow",
    "bright-blue",
    "bright-magenta",
    "bright-cyan",
    "bright-white",
];

pub const PRESET_NAMES: [&str; 5] = ["default", "mono", "nord", "solarized-dark", "gruvbox-dark"];

/// One configured color, parsed from its string form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThemeColor(pub TerminalColor);

impl ThemeColor {
    fn parse(text: &str) -> Option<TerminalColor> {
        if text == "default" {
            return Some(TerminalColor::Default);
        }
        if let Some(hex) = text.strip_prefix('#') {
            return parse_hex(hex);
        }
        if let Some(index) = ANSI_NAMES.iter().position(|name| *name == text) {
            return Some(TerminalColor::Indexed(index as u8));
        }
        // Only a bare decimal index remains. `u8::from_str` would also accept "+7", which is not
        // a form worth documenting, so require plain digits.
        if !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()) {
            return text.parse::<u8>().ok().map(TerminalColor::Indexed);
        }
        None
    }

    /// The canonical string form, so a serialized config round-trips.
    fn canonical(self) -> String {
        match self.0 {
            TerminalColor::Default => "default".to_owned(),
            TerminalColor::Indexed(index) => index.to_string(),
            TerminalColor::Rgb(red, green, blue) => format!("#{red:02x}{green:02x}{blue:02x}"),
        }
    }
}

fn parse_hex(hex: &str) -> Option<TerminalColor> {
    let expand = |value: u8| value * 17;
    match hex.len() {
        3 => {
            let mut digits = hex.chars().map(|digit| digit.to_digit(16).map(|v| v as u8));
            let red = expand(digits.next()??);
            let green = expand(digits.next()??);
            let blue = expand(digits.next()??);
            Some(TerminalColor::Rgb(red, green, blue))
        }
        6 => {
            let red = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let green = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let blue = u8::from_str_radix(&hex[4..6], 16).ok()?;
            Some(TerminalColor::Rgb(red, green, blue))
        }
        _ => None,
    }
}

impl<'de> Deserialize<'de> for ThemeColor {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        ThemeColor::parse(&text).map(ThemeColor).ok_or_else(|| {
            D::Error::invalid_value(
                Unexpected::Str(&text),
                &"\"default\", an index 0-255, an ANSI color name such as \"bright-blue\", or \"#rrggbb\"",
            )
        })
    }
}

impl Serialize for ThemeColor {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.canonical())
    }
}

/// The `[theme]` config section. Every color is optional so it can fall through to the layers
/// below it; see the module comment.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Theme {
    pub preset: Option<String>,
    pub active_frame: Option<ThemeColor>,
    pub inactive_frame: Option<ThemeColor>,
    pub active_title: Option<ThemeColor>,
    pub inactive_title: Option<ThemeColor>,
    pub frame_background: Option<ThemeColor>,
    pub status_foreground: Option<ThemeColor>,
    pub status_background: Option<ThemeColor>,
    pub search_match_foreground: Option<ThemeColor>,
    pub search_match_background: Option<ThemeColor>,
    pub search_current_foreground: Option<ThemeColor>,
    pub search_current_background: Option<ThemeColor>,
    pub sync_indicator: Option<ThemeColor>,
    /// Whether the status bar paints its background across the full width. Without it the bar is
    /// only as wide as its text, which looks broken against a colored background.
    pub status_fill: Option<bool>,
}

/// A theme with every color decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedTheme {
    pub active_frame: TerminalColor,
    pub inactive_frame: TerminalColor,
    pub active_title: TerminalColor,
    pub inactive_title: TerminalColor,
    pub frame_background: TerminalColor,
    pub status_foreground: TerminalColor,
    pub status_background: TerminalColor,
    pub search_match_foreground: TerminalColor,
    pub search_match_background: TerminalColor,
    pub search_current_foreground: TerminalColor,
    pub search_current_background: TerminalColor,
    pub sync_indicator: TerminalColor,
    pub status_fill: bool,
}

impl ResolvedTheme {
    pub fn frame(&self, focused: bool) -> FrameStyle {
        FrameStyle {
            border: if focused {
                self.active_frame
            } else {
                self.inactive_frame
            },
            title: if focused {
                self.active_title
            } else {
                self.inactive_title
            },
            background: self.frame_background,
        }
    }

    pub fn status(&self) -> TextStyle {
        TextStyle {
            foreground: self.status_foreground,
            background: self.status_background,
        }
    }

    pub fn search_match(&self) -> TextStyle {
        TextStyle {
            foreground: self.search_match_foreground,
            background: self.search_match_background,
        }
    }

    pub fn search_current(&self) -> TextStyle {
        TextStyle {
            foreground: self.search_current_foreground,
            background: self.search_current_background,
        }
    }

    pub fn sync_indicator(&self) -> TextStyle {
        TextStyle {
            foreground: self.sync_indicator,
            background: self.status_background,
        }
    }
}

/// The built-in look: exactly the colors vvmux used before themes existed.
fn base() -> ResolvedTheme {
    ResolvedTheme {
        active_frame: TerminalColor::Indexed(12),
        inactive_frame: TerminalColor::Indexed(8),
        active_title: TerminalColor::Indexed(12),
        inactive_title: TerminalColor::Indexed(8),
        frame_background: TerminalColor::Default,
        status_foreground: TerminalColor::Indexed(15),
        status_background: TerminalColor::Indexed(4),
        search_match_foreground: TerminalColor::Indexed(0),
        search_match_background: TerminalColor::Indexed(11),
        search_current_foreground: TerminalColor::Indexed(15),
        search_current_background: TerminalColor::Indexed(5),
        sync_indicator: TerminalColor::Indexed(14),
        status_fill: true,
    }
}

const fn rgb(red: u8, green: u8, blue: u8) -> TerminalColor {
    TerminalColor::Rgb(red, green, blue)
}

/// A named preset, or `None` when the name is not one of [`PRESET_NAMES`].
pub fn preset(name: &str) -> Option<ResolvedTheme> {
    let theme = match name {
        "default" => base(),
        // Indexed only, for terminals with a palette the user has already tuned.
        "mono" => ResolvedTheme {
            active_frame: TerminalColor::Indexed(7),
            inactive_frame: TerminalColor::Indexed(8),
            active_title: TerminalColor::Indexed(15),
            inactive_title: TerminalColor::Indexed(8),
            frame_background: TerminalColor::Default,
            status_foreground: TerminalColor::Indexed(0),
            status_background: TerminalColor::Indexed(7),
            search_match_foreground: TerminalColor::Indexed(0),
            search_match_background: TerminalColor::Indexed(7),
            search_current_foreground: TerminalColor::Indexed(15),
            search_current_background: TerminalColor::Indexed(8),
            sync_indicator: TerminalColor::Indexed(15),
            status_fill: true,
        },
        "nord" => ResolvedTheme {
            active_frame: rgb(0x88, 0xc0, 0xd0),
            inactive_frame: rgb(0x4c, 0x56, 0x6a),
            active_title: rgb(0xec, 0xef, 0xf4),
            inactive_title: rgb(0x61, 0x6e, 0x88),
            frame_background: TerminalColor::Default,
            status_foreground: rgb(0xec, 0xef, 0xf4),
            status_background: rgb(0x3b, 0x42, 0x52),
            search_match_foreground: rgb(0x2e, 0x34, 0x40),
            search_match_background: rgb(0xeb, 0xcb, 0x8b),
            search_current_foreground: rgb(0x2e, 0x34, 0x40),
            search_current_background: rgb(0xd0, 0x87, 0x70),
            sync_indicator: rgb(0xa3, 0xbe, 0x8c),
            status_fill: true,
        },
        "solarized-dark" => ResolvedTheme {
            active_frame: rgb(0x26, 0x8b, 0xd2),
            inactive_frame: rgb(0x58, 0x6e, 0x75),
            active_title: rgb(0x93, 0xa1, 0xa1),
            inactive_title: rgb(0x58, 0x6e, 0x75),
            frame_background: TerminalColor::Default,
            status_foreground: rgb(0x93, 0xa1, 0xa1),
            status_background: rgb(0x07, 0x36, 0x42),
            search_match_foreground: rgb(0x00, 0x2b, 0x36),
            search_match_background: rgb(0xb5, 0x89, 0x00),
            search_current_foreground: rgb(0x00, 0x2b, 0x36),
            search_current_background: rgb(0xcb, 0x4b, 0x16),
            sync_indicator: rgb(0x85, 0x99, 0x00),
            status_fill: true,
        },
        "gruvbox-dark" => ResolvedTheme {
            active_frame: rgb(0xfa, 0xbd, 0x2f),
            inactive_frame: rgb(0x66, 0x5c, 0x54),
            active_title: rgb(0xeb, 0xdb, 0xb2),
            inactive_title: rgb(0x92, 0x83, 0x74),
            frame_background: TerminalColor::Default,
            status_foreground: rgb(0xeb, 0xdb, 0xb2),
            status_background: rgb(0x3c, 0x38, 0x36),
            search_match_foreground: rgb(0x28, 0x28, 0x28),
            search_match_background: rgb(0xfa, 0xbd, 0x2f),
            search_current_foreground: rgb(0x28, 0x28, 0x28),
            search_current_background: rgb(0xfe, 0x80, 0x19),
            sync_indicator: rgb(0xb8, 0xbb, 0x26),
            status_fill: true,
        },
        _ => return None,
    };
    Some(theme)
}

/// Apply the precedence described in the module comment.
///
/// `appearance` is the deprecated section; only its four keys have a legacy analogue, and each is
/// consulted only when the corresponding `[theme]` key is absent.
pub fn resolve(theme: &Theme, appearance: &Appearance) -> ResolvedTheme {
    let mut resolved = theme
        .preset
        .as_deref()
        .and_then(preset)
        .unwrap_or_else(base);

    let legacy = |value: Option<u8>| value.map(TerminalColor::Indexed);
    let pick = |explicit: Option<ThemeColor>, legacy: Option<TerminalColor>, current| {
        explicit.map(|color| color.0).or(legacy).unwrap_or(current)
    };

    // A legacy frame color moves the title with it, so `[appearance]` alone keeps looking exactly
    // as it did when the title was drawn in the border color.
    let active_legacy = legacy(appearance.active_frame);
    let inactive_legacy = legacy(appearance.inactive_frame);

    resolved.active_frame = pick(theme.active_frame, active_legacy, resolved.active_frame);
    resolved.inactive_frame = pick(
        theme.inactive_frame,
        inactive_legacy,
        resolved.inactive_frame,
    );
    resolved.active_title = pick(theme.active_title, active_legacy, resolved.active_title);
    resolved.inactive_title = pick(
        theme.inactive_title,
        inactive_legacy,
        resolved.inactive_title,
    );
    resolved.frame_background = pick(theme.frame_background, None, resolved.frame_background);
    resolved.status_foreground = pick(
        theme.status_foreground,
        legacy(appearance.status_foreground),
        resolved.status_foreground,
    );
    resolved.status_background = pick(
        theme.status_background,
        legacy(appearance.status_background),
        resolved.status_background,
    );
    resolved.search_match_foreground = pick(
        theme.search_match_foreground,
        None,
        resolved.search_match_foreground,
    );
    resolved.search_match_background = pick(
        theme.search_match_background,
        None,
        resolved.search_match_background,
    );
    resolved.search_current_foreground = pick(
        theme.search_current_foreground,
        None,
        resolved.search_current_foreground,
    );
    resolved.search_current_background = pick(
        theme.search_current_background,
        None,
        resolved.search_current_background,
    );
    resolved.sync_indicator = pick(theme.sync_indicator, None, resolved.sync_indicator);
    resolved.status_fill = theme.status_fill.unwrap_or(resolved.status_fill);
    resolved
}

#[cfg(test)]
mod tests {
    use super::*;

    fn color(text: &str) -> Option<TerminalColor> {
        ThemeColor::parse(text)
    }

    #[test]
    fn every_documented_color_form_parses() {
        assert_eq!(color("default"), Some(TerminalColor::Default));
        assert_eq!(color("0"), Some(TerminalColor::Indexed(0)));
        assert_eq!(color("255"), Some(TerminalColor::Indexed(255)));
        assert_eq!(color("black"), Some(TerminalColor::Indexed(0)));
        assert_eq!(color("bright-blue"), Some(TerminalColor::Indexed(12)));
        assert_eq!(color("bright-white"), Some(TerminalColor::Indexed(15)));
        assert_eq!(color("#0a0b0c"), Some(TerminalColor::Rgb(10, 11, 12)));
        assert_eq!(
            color("#abc"),
            Some(TerminalColor::Rgb(0xaa, 0xbb, 0xcc)),
            "the three-digit form expands each nibble"
        );
    }

    #[test]
    fn malformed_colors_are_rejected() {
        for text in [
            "", "256", "-1", "+7", "7.0", "#", "#12", "#12345", "#gggggg", "blurple", "Default",
            "BLACK",
        ] {
            assert!(color(text).is_none(), "{text} must not parse");
        }
    }

    #[test]
    fn colors_round_trip_through_their_canonical_form() {
        for text in ["default", "12", "#0a0b0c"] {
            let parsed = ThemeColor(color(text).unwrap());
            assert_eq!(parsed.canonical(), text);
            assert_eq!(color(&parsed.canonical()), Some(parsed.0));
        }
        // Named and short forms canonicalize to their expanded equivalents.
        assert_eq!(ThemeColor(color("bright-blue").unwrap()).canonical(), "12");
        assert_eq!(ThemeColor(color("#abc").unwrap()).canonical(), "#aabbcc");
    }

    #[test]
    fn an_empty_config_keeps_the_pre_theme_colors() {
        let resolved = resolve(&Theme::default(), &Appearance::default());

        assert_eq!(resolved.active_frame, TerminalColor::Indexed(12));
        assert_eq!(resolved.inactive_frame, TerminalColor::Indexed(8));
        assert_eq!(resolved.status_foreground, TerminalColor::Indexed(15));
        assert_eq!(resolved.status_background, TerminalColor::Indexed(4));
        assert_eq!(resolved.search_match_background, TerminalColor::Indexed(11));
        assert_eq!(
            resolved.search_current_background,
            TerminalColor::Indexed(5)
        );
        assert_eq!(resolved.frame_background, TerminalColor::Default);
        assert_eq!(
            resolved.active_title, resolved.active_frame,
            "a title with no theme keeps being drawn in the border color"
        );
    }

    #[test]
    fn the_deprecated_appearance_section_still_applies() {
        let appearance = Appearance {
            active_frame: Some(3),
            inactive_frame: Some(4),
            status_foreground: Some(5),
            status_background: Some(6),
        };
        let resolved = resolve(&Theme::default(), &appearance);

        assert_eq!(resolved.active_frame, TerminalColor::Indexed(3));
        assert_eq!(resolved.inactive_frame, TerminalColor::Indexed(4));
        assert_eq!(resolved.status_foreground, TerminalColor::Indexed(5));
        assert_eq!(resolved.status_background, TerminalColor::Indexed(6));
        assert_eq!(
            resolved.active_title,
            TerminalColor::Indexed(3),
            "a legacy frame color must carry its title with it"
        );
    }

    #[test]
    fn theme_beats_appearance_beats_preset_beats_default() {
        let appearance = Appearance {
            active_frame: Some(3),
            inactive_frame: Some(4),
            status_foreground: None,
            status_background: None,
        };
        let theme = Theme {
            preset: Some("nord".into()),
            active_frame: Some(ThemeColor(TerminalColor::Rgb(1, 2, 3))),
            search_current_background: Some(ThemeColor(TerminalColor::Rgb(4, 5, 6))),
            ..Theme::default()
        };
        let resolved = resolve(&theme, &appearance);

        assert_eq!(
            resolved.active_frame,
            TerminalColor::Rgb(1, 2, 3),
            "an explicit theme key wins"
        );
        assert_eq!(
            resolved.inactive_frame,
            TerminalColor::Indexed(4),
            "appearance wins over the preset where theme is silent"
        );
        assert_eq!(
            resolved.status_background,
            preset("nord").unwrap().status_background,
            "the preset supplies what neither section set"
        );
        assert_eq!(
            resolved.search_current_background,
            TerminalColor::Rgb(4, 5, 6)
        );
    }

    #[test]
    fn a_preset_alone_replaces_every_default() {
        for name in PRESET_NAMES {
            let theme = Theme {
                preset: Some(name.to_owned()),
                ..Theme::default()
            };
            let resolved = resolve(&theme, &Appearance::default());
            assert_eq!(
                resolved,
                preset(name).unwrap(),
                "{name} must survive resolution untouched"
            );
        }
        assert!(preset("no-such-preset").is_none());
    }

    #[test]
    fn status_fill_is_opt_out() {
        assert!(resolve(&Theme::default(), &Appearance::default()).status_fill);

        let theme = Theme {
            status_fill: Some(false),
            ..Theme::default()
        };
        assert!(!resolve(&theme, &Appearance::default()).status_fill);
    }

    #[test]
    fn frame_and_status_styles_follow_focus() {
        let resolved = resolve(&Theme::default(), &Appearance::default());

        assert_eq!(resolved.frame(true).border, resolved.active_frame);
        assert_eq!(resolved.frame(false).border, resolved.inactive_frame);
        assert_eq!(
            resolved.frame(true).background,
            resolved.frame_background,
            "both focus states share the frame background"
        );
        assert_eq!(resolved.status().foreground, resolved.status_foreground);
        assert_eq!(
            resolved.sync_indicator().foreground,
            resolved.sync_indicator
        );
        assert_eq!(
            resolved.sync_indicator().background,
            resolved.status_background
        );
    }
}
