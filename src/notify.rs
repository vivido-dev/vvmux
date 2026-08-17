//! Desktop notifications rendered into the outer terminal by the foreground client.
//!
//! Backend detection and the OSC 9 / OSC 99 encodings are adapted from HerdR commit
//! `6c6ddcd49384d6ea9f0ee2e63bf7b2643dfd5bcf` (`src/terminal_notify.rs`, Apache-2.0). See
//! `agent/PROVENANCE.md`.
//!
//! This lives in the client, not the session: only the process attached to the user's terminal
//! knows what that terminal is, and the token-ownership rule keeps that knowledge out of the
//! hidden server.

/// Terminals with a notification escape vvmux is willing to emit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationBackend {
    /// OSC 9, the single-string form.
    Osc9,
    /// OSC 99, kitty's structured title/body form.
    Osc99,
}

/// Identify the outer terminal from the environment the client was started with.
///
/// Returns `None` when nothing is recognized. vvmux then emits nothing at all rather than
/// guessing: an unrecognized terminal prints an unknown escape as garbage into the user's session,
/// which is worse than a missed notification.
pub fn detect_backend() -> Option<NotificationBackend> {
    backend_for(
        std::env::var("TERM_PROGRAM").ok().as_deref(),
        std::env::var("TERM").ok().as_deref(),
        std::env::var_os("KITTY_WINDOW_ID").is_some(),
    )
}

fn backend_for(
    term_program: Option<&str>,
    term: Option<&str>,
    kitty_window: bool,
) -> Option<NotificationBackend> {
    match term_program {
        Some("ghostty" | "iTerm.app" | "WezTerm") => return Some(NotificationBackend::Osc9),
        Some("vivido") => return Some(NotificationBackend::Osc9),
        _ => {}
    }
    if kitty_window {
        return Some(NotificationBackend::Osc99);
    }
    match term {
        Some("xterm-ghostty") => Some(NotificationBackend::Osc9),
        Some("xterm-kitty") => Some(NotificationBackend::Osc99),
        Some(term) if term.contains("wezterm") => Some(NotificationBackend::Osc9),
        _ => None,
    }
}

/// Encode a notification for the detected backend, or `None` when there is none.
pub fn notification_sequence(title: &str, body: Option<&str>) -> Option<Vec<u8>> {
    let sequence = match detect_backend()? {
        NotificationBackend::Osc9 => osc9(title, body),
        NotificationBackend::Osc99 => osc99(title, body),
    };
    Some(if std::env::var_os("TMUX").is_some() {
        wrap_tmux_passthrough(&sequence)
    } else {
        sequence
    })
}

fn osc9(title: &str, body: Option<&str>) -> Vec<u8> {
    let message = match body.filter(|body| !body.is_empty()) {
        Some(body) => format!("{}: {}", sanitize(title), sanitize(body)),
        None => sanitize(title),
    };
    format!("\x1b]9;{message}\x1b\\").into_bytes()
}

fn osc99(title: &str, body: Option<&str>) -> Vec<u8> {
    let title = sanitize(title);
    match body.filter(|body| !body.is_empty()) {
        Some(body) => {
            let body = sanitize(body);
            format!("\x1b]99;i=1:d=0;{title}\x1b\\\x1b]99;i=1:p=body;{body}\x1b\\").into_bytes()
        }
        None => format!("\x1b]99;;{title}\x1b\\").into_bytes(),
    }
}

/// Strip anything that could terminate the escape early or move the cursor.
///
/// The text originates in a pane's own output, so it is attacker-controlled in exactly the way an
/// OSC payload must not be.
fn sanitize(text: &str) -> String {
    text.chars()
        .filter(|character| !matches!(character, '\u{1b}' | '\u{7}' | '\u{9c}'))
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect()
}

/// tmux swallows escapes it does not recognize unless they are wrapped and doubled.
fn wrap_tmux_passthrough(sequence: &[u8]) -> Vec<u8> {
    let mut wrapped = Vec::with_capacity(sequence.len() + 16);
    wrapped.extend_from_slice(b"\x1bPtmux;");
    for &byte in sequence {
        if byte == 0x1b {
            wrapped.push(0x1b);
        }
        wrapped.push(byte);
    }
    wrapped.extend_from_slice(b"\x1b\\");
    wrapped
}

/// Run the configured sound command detached, ignoring its output and exit status.
///
/// Best-effort by design: a missing or failing player must not disturb the session, and the client
/// must never wait on it.
pub fn play_sound(command: &[String]) {
    let Some((program, arguments)) = command.split_first() else {
        return;
    };
    let _ = std::process::Command::new(program)
        .args(arguments)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backends_are_recognized_and_unknown_terminals_get_nothing() {
        assert_eq!(
            backend_for(Some("ghostty"), None, false),
            Some(NotificationBackend::Osc9)
        );
        assert_eq!(
            backend_for(Some("iTerm.app"), None, false),
            Some(NotificationBackend::Osc9)
        );
        assert_eq!(
            backend_for(None, Some("xterm-kitty"), false),
            Some(NotificationBackend::Osc99)
        );
        assert_eq!(
            backend_for(None, None, true),
            Some(NotificationBackend::Osc99)
        );
        assert_eq!(
            backend_for(None, Some("wezterm-256color"), false),
            Some(NotificationBackend::Osc9)
        );
        // An unrecognized terminal emits nothing rather than an escape it may print literally.
        assert_eq!(backend_for(None, Some("dumb"), false), None);
        assert_eq!(backend_for(Some("Apple_Terminal"), None, false), None);
    }

    #[test]
    fn payloads_cannot_terminate_their_own_escape() {
        // A pane controls this text, so a payload that closed the OSC early could write arbitrary
        // escapes into the user's real terminal.
        let hostile = "done\u{1b}]0;pwned\u{7}\nnext";
        let sequence = String::from_utf8(osc9(hostile, None)).unwrap();
        assert!(sequence.starts_with("\x1b]9;"));
        assert!(sequence.ends_with("\x1b\\"));
        assert_eq!(sequence.matches('\u{1b}').count(), 2);
        assert!(!sequence.contains('\u{7}'));
        assert!(!sequence.contains('\n'));
    }

    #[test]
    fn kitty_splits_title_and_body_while_osc9_joins_them() {
        let kitty = String::from_utf8(osc99("codex blocked", Some("pane 2"))).unwrap();
        assert!(kitty.contains("]99;i=1:d=0;codex blocked"));
        assert!(kitty.contains("]99;i=1:p=body;pane 2"));

        let osc9 = String::from_utf8(osc9("codex blocked", Some("pane 2"))).unwrap();
        assert!(osc9.contains("codex blocked: pane 2"));

        // An empty body is not a body.
        let bare = String::from_utf8(super::osc9("codex blocked", Some(""))).unwrap();
        assert!(bare.ends_with("codex blocked\x1b\\"));
    }

    #[test]
    fn tmux_passthrough_doubles_escapes() {
        assert_eq!(
            wrap_tmux_passthrough(b"\x1b]9;hi\x1b\\"),
            b"\x1bPtmux;\x1b\x1b]9;hi\x1b\x1b\\\x1b\\"
        );
    }

    #[test]
    fn an_empty_sound_command_is_not_run() {
        // Guards the default config: no command configured must not mean "spawn nothing-named".
        play_sound(&[]);
    }
}
