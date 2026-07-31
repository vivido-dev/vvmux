#![cfg(unix)]

//! Losing the window's focus must not type into a pane.
//!
//! The client asks its own terminal for focus reports so the session can pass them on. Those
//! reports used to be forwarded as ordinary input, so blurring the window wrote `ESC[O` into
//! whichever pane was focused: a shell prompt answered with a stray newline, and a program that
//! never asked for the mode echoed `^[[O`. Focus now reaches only a pane whose program enabled
//! focus reporting, and only when that pane's own focus changed.

use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use vvmux_terminal::pty::PtyProcess;

struct SessionGuard {
    executable: PathBuf,
    name: String,
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        let _ = Command::new(&self.executable)
            .args(["kill-session", "-t", &self.name])
            .output();
    }
}

#[test]
fn focus_reports_reach_only_the_pane_that_asked_for_them() {
    let executable = PathBuf::from(env!("CARGO_BIN_EXE_vvmux"));
    let directory = tempfile::tempdir().unwrap();
    // Pane 1 asks for focus reporting; pane 2 never does. Both echo what they are sent, so a
    // report that reached the wrong pane is visible in that pane's own text.
    let shell = directory.path().join("fixture-shell");
    fs::write(
        &shell,
        br#"#!/bin/sh
# Echo control bytes as their caret form, so anything written to this pane stays visible text
# instead of being read back as an escape sequence.
stty echoctl 2>/dev/null
if [ "$VVMUX_PANE_ID" = "1" ]; then
    printf '\033[?1004h'
fi
printf 'READY pane=%s\n' "$VVMUX_PANE_ID"
while IFS= read -r line; do
    printf 'OUT pane=%s:%s\n' "$VVMUX_PANE_ID" "$line"
done
"#,
    )
    .unwrap();
    fs::set_permissions(&shell, fs::Permissions::from_mode(0o700)).unwrap();
    let config = directory.path().join("vvmux.toml");
    fs::write(
        &config,
        format!(
            "[general]\nshell = \"{}\"\nrender_interval_ms = 1\n",
            shell.display()
        ),
    )
    .unwrap();

    let session = format!(
        "focus-probe-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let guard = SessionGuard {
        executable: executable.clone(),
        name: session.clone(),
    };

    // The client only reads focus reports from its own terminal, so the session is hosted in a
    // PTY the test can write raw bytes into.
    let parts = PtyProcess::spawn(
        std::ffi::OsStr::new("/bin/sh"),
        directory.path(),
        100,
        30,
        &[],
    )
    .unwrap();
    let control = parts.control.clone();
    let mut reader = parts.reader;
    let (sender, receiver) = mpsc::channel();
    let reader_thread = std::thread::spawn(move || {
        let mut buffer = [0_u8; 8192];
        while let Ok(read) = reader.read(&mut buffer) {
            if read == 0 || sender.send(buffer[..read].to_vec()).is_err() {
                break;
            }
        }
    });
    let attach = format!(
        "exec {} --config {} new -s {session}\n",
        executable.display(),
        config.display()
    );
    parts.input.send(attach.as_bytes()).unwrap();
    let mut transcript = Vec::new();
    assert!(
        wait_for(
            &receiver,
            &mut transcript,
            b"\x1b[?1049h",
            Duration::from_secs(15)
        ),
        "the client never entered the alternate screen"
    );

    wait_for_text(&executable, &session, 1, "READY pane=1");
    wait_for_mode(&executable, &session, 1, "focus_reporting");
    assert_success(&command(
        &executable,
        &session,
        &["split", "vertical", "--pane-id", "1"],
    ));
    wait_for_text(&executable, &session, 2, "READY pane=2");

    // The split focused pane 2, so pane 1 has already been told it lost focus.
    wait_for_text(&executable, &session, 1, "^[[O");

    assert_success(&command(
        &executable,
        &session,
        &["focus", "--pane-id", "1"],
    ));
    wait_for_text(&executable, &session, 1, "^[[O^[[I");

    // A blurred and refocused window is reported to the focused pane that asked for it.
    parts.input.send(b"\x1b[O").unwrap();
    wait_for_text(&executable, &session, 1, "^[[O^[[I^[[O");
    parts.input.send(b"\x1b[I").unwrap();
    wait_for_text(&executable, &session, 1, "^[[O^[[I^[[O^[[I");

    // Pane 2 holds focus for the rest of the run and never asked for focus reporting.
    assert_success(&command(
        &executable,
        &session,
        &["focus", "--pane-id", "2"],
    ));
    wait_for_text(&executable, &session, 1, "^[[O^[[I^[[O^[[I^[[O");
    parts.input.send(b"\x1b[O").unwrap();
    parts.input.send(b"\x1b[I").unwrap();
    // Ordinary input still reaches the focused pane, and arrives after the discarded reports.
    parts.input.send(b"typed\r").unwrap();
    wait_for_text(&executable, &session, 2, "OUT pane=2:typed");

    let pane_two = pane_text(&executable, &session, 2);
    assert!(
        !pane_two.contains("^["),
        "a pane that never enabled focus reporting was sent one: {pane_two:?}"
    );
    let pane_one = pane_text(&executable, &session, 1);
    assert!(
        !pane_one.contains("^[[O^[[O") && !pane_one.contains("^[[I^[[I"),
        "focus was reported twice without an intervening change: {pane_one:?}"
    );

    drop(guard);
    control.terminate_blocking();
    drop(receiver);
    reader_thread.join().unwrap();
}

fn wait_for(
    receiver: &mpsc::Receiver<Vec<u8>>,
    transcript: &mut Vec<u8>,
    needle: &[u8],
    timeout: Duration,
) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if transcript
            .windows(needle.len())
            .any(|window| window == needle)
        {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        if let Ok(chunk) = receiver.recv_timeout(Duration::from_millis(100)) {
            transcript.extend(chunk);
        }
    }
}

fn wait_for_text(executable: &Path, session: &str, pane: u64, text: &str) {
    let output = command(
        executable,
        session,
        &[
            "wait",
            "text",
            text,
            "--pane-id",
            &pane.to_string(),
            "--timeout",
            "15s",
        ],
    );
    assert!(
        output.status.success(),
        "pane {pane} never showed {text:?}: {}",
        pane_text(executable, session, pane)
    );
}

fn wait_for_mode(executable: &Path, session: &str, pane: u64, mode: &str) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let output = command(
            executable,
            session,
            &["inspect", "--pane-id", &pane.to_string()],
        );
        if output.status.success() {
            let value: Value = serde_json::from_slice(&output.stdout).unwrap();
            if value["pane"]["modes"]
                .as_array()
                .is_some_and(|modes| modes.iter().any(|name| name == mode))
            {
                return;
            }
        }
        assert!(
            Instant::now() < deadline,
            "pane {pane} never entered {mode}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn pane_text(executable: &Path, session: &str, pane: u64) -> String {
    let output = command(
        executable,
        session,
        &["get-text", "--pane-id", &pane.to_string()],
    );
    assert_success(&output);
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn command(executable: &Path, session: &str, arguments: &[&str]) -> Output {
    Command::new(executable)
        .args(["msg", "--target", session])
        .args(arguments)
        .output()
        .unwrap()
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
