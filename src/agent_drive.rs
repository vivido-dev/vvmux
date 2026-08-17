//! Shared machinery for driving an agent inside a pane: deciding whether a pane is an *available
//! shell*, and turning an argument vector into a command line that shell will run verbatim.
//!
//! Launching an agent means typing at a shell, not spawning a process, so both halves have to be
//! right for the same reason: whatever this module produces is executed by whatever program owns
//! the pane's foreground. A wrong availability answer types a command line into an editor; a wrong
//! quote lets an argument split into two.
//!
//! Adapted from herdr (Apache-2.0, commit `6c6ddcd`) — see `crate::agent`'s PROVENANCE.md.
//!
//! Shared by `agent-start`, `agent-prompt`, and session restore, none of which should re-derive
//! process-table or shell-quoting rules.

use std::time::Duration;

/// How long after typing an agent's command before detection is allowed to conclude a launch
/// failed.
///
/// An agent takes a moment to replace the shell and paint its first screen, and until it does the
/// pane still looks like the shell it was. Without this, every launch would report failure in its
/// first instants; with it, "no agent yet" only becomes an answer after the agent has had time to
/// appear. Matches herdr's `AGENT_START_SETTLE_DELAY`.
pub const AGENT_START_SETTLE: Duration = Duration::from_secs(3);
/// Floor for an `agent-start` readiness timeout: below the settle delay, a launch could only ever
/// time out.
pub const AGENT_START_MIN_TIMEOUT: Duration = AGENT_START_SETTLE;
/// Ceiling for an `agent-start` readiness timeout.
pub const AGENT_START_MAX_TIMEOUT: Duration = Duration::from_secs(300);
/// Arguments one launch may pass through to an agent.
pub const MAX_AGENT_START_ARGS: usize = 32;
/// Bytes in one launch argument.
pub const MAX_AGENT_START_ARG_BYTES: usize = 4096;
/// Delay between prompt submit text and delayed Enter.
///
/// Full-screen agents can absorb Enter while it arrives in the same paste packet, so the text and
/// submit key are intentionally separated in time.
pub const AGENT_PROMPT_SUBMIT_DELAY: Duration = Duration::from_millis(300);
/// Maximum stall window before `agent-prompt` marks the transition as stalled.
pub const AGENT_PROMPT_EFFECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Shells whose foreground prompt will run a typed command line.
///
/// `cmd` is deliberately absent, unlike herdr's list: this module quotes POSIX-style or
/// PowerShell-style, and `cmd.exe` accepts neither. Refusing is honest; quoting for a shell whose
/// rules we do not implement would fail silently at the point the command runs.
const PANE_SHELLS: &[&str] = &[
    "sh",
    "bash",
    "dash",
    "zsh",
    "fish",
    "ksh",
    "mksh",
    "csh",
    "tcsh",
    "elvish",
    "xonsh",
    "nu",
    "pwsh",
    "powershell",
];

/// One process in a pane's foreground job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForegroundProcess {
    pub pid: u32,
    pub name: String,
}

/// Strip a process name down to the token the shell tables are keyed by.
///
/// A login shell arrives as `-zsh`, an absolute path as `/bin/bash`, and a Windows binary as
/// `pwsh.exe`; all three name the same shell.
fn normalized_process_name(name: &str) -> String {
    let base = name
        .rsplit(['/', '\\'])
        .find(|part| !part.is_empty())
        .unwrap_or(name)
        .trim_start_matches('-');
    base.strip_suffix(".exe")
        .unwrap_or(base)
        .to_ascii_lowercase()
}

pub fn is_pane_shell(name: &str) -> bool {
    PANE_SHELLS.contains(&normalized_process_name(name).as_str())
}

fn is_powershell(name: &str) -> bool {
    matches!(
        normalized_process_name(name).as_str(),
        "pwsh" | "powershell"
    )
}

/// The shell that owns this pane's foreground, or `None` when the pane is not available.
///
/// Available means the job is exactly one process, that process is the pane's own child, and it is
/// a recognized shell. Anything else — a running command, a backgrounded job, an editor, an agent
/// already there — means typing would land somewhere the caller did not intend.
///
/// `group` is the terminal's foreground process group where the platform reports one; it must then
/// be the child's own group, which is what distinguishes "the shell is waiting at its prompt" from
/// "the shell is waiting on a command it started". Where the platform cannot report a group
/// (Windows), `None` leaves the one-process-and-it-is-the-child rule to carry the check, since
/// there the job is collected by walking the child's descendants.
pub fn available_pane_shell(
    child_pid: u32,
    group: Option<u32>,
    job: &[ForegroundProcess],
) -> Option<String> {
    if child_pid == 0 || group.is_some_and(|group| group != child_pid) {
        return None;
    }
    let [process] = job else {
        return None;
    };
    (process.pid == child_pid && is_pane_shell(&process.name)).then(|| process.name.clone())
}

/// Quote one argument for a POSIX shell.
fn quote_posix(value: &str) -> String {
    if !value.is_empty()
        && value.chars().all(|ch| {
            ch.is_ascii_alphanumeric()
                || matches!(
                    ch,
                    '@' | '%' | '_' | '+' | '=' | ':' | ',' | '.' | '/' | '-'
                )
        })
    {
        return value.to_owned();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Quote one argument for PowerShell.
fn quote_powershell(value: &str) -> String {
    if !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'_' | b'-' | b'.' | b'/' | b':' | b'+' | b'=')
        })
    {
        return value.to_owned();
    }
    format!("'{}'", value.replace('\'', "''"))
}

/// Render `argv` as a command line the named shell will parse back into the same arguments.
pub fn shell_command_line(argv: &[String], shell_name: &str) -> Option<String> {
    let quote = if is_powershell(shell_name) {
        quote_powershell
    } else {
        quote_posix
    };
    let mut parts = argv.iter();
    let mut command = quote(parts.next()?);
    for part in parts {
        command.push(' ');
        command.push_str(&quote(part));
    }
    Some(command)
}

/// Encode one SGR (1006) mouse report.
///
/// Shared by the client's mouse forwarding and by transcript reads that drive an application's own
/// wheel interface, so a synthesized wheel event is byte-identical to a real one.
pub fn encode_sgr_mouse(button: u16, column: u32, row: u32, press: bool) -> String {
    let terminator = if press { 'M' } else { 'm' };
    format!("\x1b[<{button};{column};{row}{terminator}")
}

/// The SGR button code for a wheel click. Wheel buttons carry bit 64; 0 is up, 1 is down.
pub const WHEEL_UP: u16 = 64;
pub const WHEEL_DOWN: u16 = 65;

#[cfg(test)]
mod tests {
    use super::*;

    fn process(pid: u32, name: &str) -> ForegroundProcess {
        ForegroundProcess {
            pid,
            name: name.to_owned(),
        }
    }

    #[test]
    fn shell_names_normalize_before_matching() {
        // A login shell, an absolute path, and a Windows binary all name the same shell.
        for name in ["zsh", "-zsh", "/bin/zsh", "/usr/local/bin/ZSH"] {
            assert!(is_pane_shell(name), "{name} should be a pane shell");
        }
        assert!(is_pane_shell("pwsh.exe"));
        assert!(is_pane_shell(r"C:\Program Files\PowerShell\pwsh.exe"));
        // `cmd` quotes like neither shell this module can produce, so it is refused rather than
        // mis-quoted at the point the command finally runs.
        assert!(!is_pane_shell("cmd"));
        assert!(!is_pane_shell("cmd.exe"));
        assert!(!is_pane_shell("vim"));
        assert!(!is_pane_shell("claude"));
        assert!(!is_pane_shell(""));
    }

    #[test]
    fn a_pane_is_available_only_when_its_own_shell_is_alone_in_the_foreground() {
        assert_eq!(
            available_pane_shell(42, Some(42), &[process(42, "-bash")]),
            Some("-bash".to_owned())
        );
        // A foreground group that is not the pane's child is a running command, not the shell.
        assert_eq!(
            available_pane_shell(42, Some(99), &[process(99, "bash")]),
            None
        );
        // The shell is there, but so is something else: a pipeline or a job is in the foreground.
        assert_eq!(
            available_pane_shell(42, Some(42), &[process(42, "bash"), process(43, "less")]),
            None
        );
        // The group leader is the pane's child, but it is no longer a shell — an exec'd agent.
        assert_eq!(
            available_pane_shell(42, Some(42), &[process(42, "claude")]),
            None
        );
        assert_eq!(available_pane_shell(42, Some(42), &[]), None);
        // Windows reports no foreground group and collects the job by walking the child's
        // descendants, so an absent group is not by itself a refusal — the one-process rule and
        // the child-pid match still have to hold.
        assert_eq!(
            available_pane_shell(42, None, &[process(42, "pwsh.exe")]),
            Some("pwsh.exe".to_owned())
        );
        assert_eq!(
            available_pane_shell(42, None, &[process(42, "pwsh.exe"), process(43, "git.exe")]),
            None
        );
        assert_eq!(available_pane_shell(0, None, &[]), None);
    }

    #[test]
    fn posix_quoting_survives_a_round_trip_through_sh() {
        assert_eq!(
            shell_command_line(&["claude".into()], "bash").unwrap(),
            "claude"
        );
        assert_eq!(
            shell_command_line(&["codex".into(), "--model".into(), "gpt-5.4".into()], "zsh")
                .unwrap(),
            "codex --model gpt-5.4"
        );
        // A space must not split the argument, and an embedded single quote must not close it.
        assert_eq!(
            shell_command_line(&["claude".into(), "review this".into()], "sh").unwrap(),
            "claude 'review this'"
        );
        assert_eq!(
            shell_command_line(&["claude".into(), "it's fine".into()], "sh").unwrap(),
            r"claude 'it'\''s fine'"
        );
        // Shell metacharacters stay data: nothing here may start a second command.
        assert_eq!(
            shell_command_line(&["claude".into(), "; rm -rf /".into()], "sh").unwrap(),
            "claude '; rm -rf /'"
        );
        assert_eq!(
            shell_command_line(&["claude".into(), "$(id)".into()], "sh").unwrap(),
            "claude '$(id)'"
        );
        assert_eq!(
            shell_command_line(&["claude".into(), String::new()], "sh").unwrap(),
            "claude ''"
        );
        assert_eq!(shell_command_line(&[], "sh"), None);
    }

    #[test]
    fn powershell_quoting_doubles_its_own_quote() {
        assert_eq!(
            shell_command_line(&["opencode".into(), "it's fine".into()], "pwsh").unwrap(),
            "opencode 'it''s fine'"
        );
        // Same argv, different shell, different escape — the shell name is what selects it.
        assert_eq!(
            shell_command_line(&["opencode".into(), "it's fine".into()], "bash").unwrap(),
            r"opencode 'it'\''s fine'"
        );
        assert_eq!(
            shell_command_line(&["opencode".into(), "a b".into()], "powershell.exe").unwrap(),
            "opencode 'a b'"
        );
    }

    #[test]
    fn sgr_mouse_encoding_matches_the_client_forwarding_format() {
        assert_eq!(encode_sgr_mouse(WHEEL_UP, 40, 12, true), "\x1b[<64;40;12M");
        assert_eq!(
            encode_sgr_mouse(WHEEL_DOWN, 40, 12, true),
            "\x1b[<65;40;12M"
        );
        assert_eq!(encode_sgr_mouse(0, 1, 1, false), "\x1b[<0;1;1m");
    }
}
