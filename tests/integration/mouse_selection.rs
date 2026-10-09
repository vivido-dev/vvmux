#![cfg(unix)]

//! A mouse selection must survive the pane continuously redrawing and scrolling.
//!
//! The selection used to be invalidated on every PTY output chunk, so selecting anything in a
//! pane that keeps printing — a redrawn TUI, `tail -f` — erased the selection (and broke a drag
//! in progress). This test drives real selections through a real attached client with raw SGR
//! mouse bytes, proves the copy plumbing end to end with a paste round-trip, and then proves the
//! highlight itself survives subsequent pane output by watching the inverse-video SGR travel
//! through the client's render stream while a background printer keeps scrolling the grid.

use crate::common;

use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use vvmux_terminal::pty::{PtyInput, PtyProcess};

struct SessionGuard {
    runtime: PathBuf,
    name: String,
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        let _ = common::vvmux_command(&self.runtime)
            .args(["kill-session", "-t", &self.name])
            .output();
    }
}

#[test]
fn double_click_copies_word_and_selection_survives_continuously_printing_pane() {
    let executable = PathBuf::from(env!("CARGO_BIN_EXE_vvmux"));
    // Short `/tmp` root: the runtime directory holds the session socket, whose path must stay
    // inside the platform's `sun_path` limit. Isolating `XDG_CONFIG_HOME` and `XDG_RUNTIME_DIR`
    // keeps a developer's own `startup.toml` and live sessions out of this test's session.
    let directory = tempfile::Builder::new()
        .prefix("vvm-")
        .tempdir_in("/tmp")
        .unwrap();
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let runtime = directory.path().to_path_buf();
    // The fixture never enables mouse reporting, so a plain left press starts a selection. `build`
    // fills the 28-row pane content and leaves "alpha bravo charlie" near the bottom, and arms a
    // background printer that starts a few seconds later and keeps scrolling the grid without any
    // further input — typing would itself dismiss the selection under test.
    let shell = directory.path().join("fixture-shell");
    fs::write(
        &shell,
        br#"#!/bin/sh
stty echoctl 2>/dev/null
printf 'READY pane=%s\n' "$VVMUX_PANE_ID"
while IFS= read -r line; do
    case "$line" in
        pixels) printf '\033[?1016hPIXELS ready\n' ;;
        cells) printf '\033[?1016lCELLS ready\n' ;;
        build)
            i=1
            while [ "$i" -le 24 ]; do printf 'line%02d\n' "$i"; i=$((i+1)); done
            printf 'alpha bravo charlie\n'
            ( sleep 6
              while :; do printf 'tick\n'; sleep 0.3; done ) &
            ;;
        *) printf 'OUT pane=%s:%s\n' "$VVMUX_PANE_ID" "$line" ;;
    esac
done
"#,
    )
    .unwrap();
    fs::set_permissions(&shell, fs::Permissions::from_mode(0o700)).unwrap();
    let config = directory.path().join("vvmux.toml");
    fs::write(
        &config,
        format!(
            "[general]\nshell = \"{}\"\nrender_interval_ms = 1\ntab_view = \"hidden\"\n",
            shell.display()
        ),
    )
    .unwrap();

    let session = format!(
        "mouse-probe-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let guard = SessionGuard {
        runtime: runtime.clone(),
        name: session.clone(),
    };

    // The client is hosted in a PTY the test writes raw bytes into, so the mouse can be driven
    // exactly the way a real terminal reports it.
    let isolation = [
        (
            "XDG_RUNTIME_DIR".to_owned(),
            runtime.to_str().unwrap().to_owned(),
        ),
        (
            "XDG_CONFIG_HOME".to_owned(),
            runtime.to_str().unwrap().to_owned(),
        ),
    ];
    let parts = PtyProcess::spawn(
        std::ffi::OsStr::new("/bin/sh"),
        None,
        directory.path(),
        100,
        30,
        &isolation,
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
            0,
            b"\x1b[?1049h",
            Duration::from_secs(15)
        ),
        "the client never entered the alternate screen: {}",
        String::from_utf8_lossy(&transcript)
    );

    // Kitty and Ghostty use one mouse encoding: resetting pixel mode (1016) selects X10,
    // even when SGR (1006) was previously enabled. The initial cell-mode sync must explicitly
    // restore SGR, or moving the mouse types the legacy report's coordinate bytes into the pane.
    assert!(
        wait_for(
            &receiver,
            &mut transcript,
            0,
            b"\x1b[?1016l\x1b[?1006h",
            Duration::from_secs(15)
        ),
        "the initial input-mode sync did not restore SGR cell reports after resetting pixel mode"
    );

    // A nested application's pixel request must still reach the host, and returning to cells
    // must restore SGR again. Then exercise hover and selection with the negotiated cell format.
    let mark = transcript.len();
    parts.input.send(b"pixels\r").unwrap();
    wait_for_text(&runtime, &session, 1, "PIXELS ready");
    assert!(
        wait_for(
            &receiver,
            &mut transcript,
            mark,
            b"\x1b[?1016h",
            Duration::from_secs(15)
        ),
        "the pane's pixel mouse request never reached the host"
    );
    let mark = transcript.len();
    parts.input.send(b"cells\r").unwrap();
    wait_for_text(&runtime, &session, 1, "CELLS ready");
    assert!(
        wait_for(
            &receiver,
            &mut transcript,
            mark,
            b"\x1b[?1016l\x1b[?1006h",
            Duration::from_secs(15)
        ),
        "returning from pixel mode did not restore SGR cell reports"
    );
    parts
        .input
        .send(b"\x1b[<35;45;10M\x1b[<35;46;11Mmouse-probe\r")
        .unwrap();
    wait_for_text(&runtime, &session, 1, "OUT pane=1:mouse-probe");
    assert!(
        !pane_text(&runtime, &session, 1).contains("^["),
        "mouse motion was forwarded as typed text"
    );

    parts.input.send(b"build\r").unwrap();
    wait_for_text(&runtime, &session, 1, "alpha bravo charlie");

    // Two ordinary clicks inside a word must select and copy it without any pointer motion or
    // Shift modifier. Prove both the clipboard update and the copy-buffer paste round-trip.
    let y = bravo_row(&runtime, &session) + 2;
    parts
        .input
        .send(format!("\x1b[<0;10;{y}M\x1b[<0;10;{y}m\x1b[<0;10;{y}M\x1b[<0;10;{y}m").as_bytes())
        .unwrap();
    assert!(
        wait_for(
            &receiver,
            &mut transcript,
            0,
            b"\x1b]52;c;YnJhdm8=\x1b\\",
            Duration::from_secs(15)
        ),
        "double-clicking bravo never copied the word to the host clipboard"
    );
    assert!(
        wait_for_inverse(&receiver, &mut transcript, 0, Duration::from_secs(15)),
        "the double-click word selection was never highlighted"
    );
    parts.input.send(b"\x02]").unwrap();
    parts.input.send(b"\r").unwrap();
    wait_for_text(&runtime, &session, 1, "OUT pane=1:bravo");

    // Holding the second click and moving onto charlie still copies only the clicked word.
    let y = bravo_row(&runtime, &session) + 2;
    let mark = transcript.len();
    parts
        .input
        .send(
            format!("\x1b[<0;10;{y}M\x1b[<0;10;{y}m\x1b[<0;10;{y}M\x1b[<32;20;{y}M\x1b[<0;20;{y}m")
                .as_bytes(),
        )
        .unwrap();
    assert!(
        wait_for(
            &receiver,
            &mut transcript,
            mark,
            b"\x1b]52;c;YnJhdm8=\x1b\\",
            Duration::from_secs(15)
        ),
        "moving during the second click extended the selected word"
    );

    // Some terminals forward Shift-left reports. Single, double and triple clicks, plus a drag,
    // must leave the existing word selection and copy buffer alone rather than force selection.
    let shifted_click = format!("\x1b[<4;4;{y}M\x1b[<4;4;{y}m");
    parts
        .input
        .send(shifted_click.repeat(3).as_bytes())
        .unwrap();
    parts
        .input
        .send(format!("\x1b[<4;4;{y}M\x1b[<36;20;{y}M\x1b[<4;20;{y}m").as_bytes())
        .unwrap();
    // Switching to Shift during a plain drag cancels it; a later ordinary release cannot copy it.
    parts
        .input
        .send(format!("\x1b[<0;4;{y}M\x1b[<36;20;{y}M\x1b[<4;20;{y}m\x1b[<0;20;{y}m").as_bytes())
        .unwrap();
    // A unique paste round-trip proves that none of those gestures overwrote the copy buffer.
    parts.input.send(b"shift-probe:\x02]\r").unwrap();
    wait_for_text(&runtime, &session, 1, "OUT pane=1:shift-probe:bravo");

    // Second selection, kept free of any keyboard input from here on.
    select_bravo(&parts.input, &runtime, &session);
    assert!(
        wait_for_inverse(&receiver, &mut transcript, 0, Duration::from_secs(15)),
        "the selection highlight never reached the client render stream"
    );

    // The background printer starts scrolling the grid all by itself. Every tick repaints the
    // rows it shifted, so once several have gone by, a stream that still carries the highlight is
    // one where the selection rotated with its text instead of being dropped by the output.
    wait_for_text(&runtime, &session, 1, "tick");
    drain_for(&receiver, &mut transcript, Duration::from_secs(2));
    let mark = transcript.len();
    assert!(
        wait_for_inverse(&receiver, &mut transcript, mark, Duration::from_secs(15)),
        "the selection highlight disappeared while the pane kept printing"
    );

    drop(guard);
    control.terminate_blocking();
    drop(receiver);
    reader_thread.join().unwrap();
}

/// Select "bravo" where it currently is.
///
/// The tiled pane draws a one-cell border, so pane content starts at display column 1 row 1;
/// "bravo" occupies pane columns 6..=10 of the "alpha bravo charlie" row. The row is resolved
/// from the pane grid at selection time, since earlier output may have scrolled it.
fn select_bravo(input: &PtyInput, runtime: &Path, session: &str) {
    // One-based SGR coordinates: pane row + border offset + 1; columns 8..=12.
    let y = bravo_row(runtime, session) + 2;
    input.send(format!("\x1b[<0;8;{y}M").as_bytes()).unwrap();
    input.send(format!("\x1b[<32;12;{y}M").as_bytes()).unwrap();
    input.send(format!("\x1b[<0;12;{y}m").as_bytes()).unwrap();
}

fn bravo_row(runtime: &Path, session: &str) -> usize {
    let output = command(runtime, session, &["get-grid", "--pane-id", "1"]);
    assert_success(&output);
    let grid: Value = serde_json::from_slice(&output.stdout).unwrap();
    grid["rows"]
        .as_array()
        .and_then(|rows| {
            rows.iter().position(|row| {
                let text = row["cells"]
                    .as_array()
                    .map(|cells| {
                        cells
                            .iter()
                            .filter_map(|cell| cell["text"].as_str())
                            .collect::<String>()
                    })
                    .unwrap_or_default();
                text.starts_with("alpha bravo charlie")
            })
        })
        .expect("the bravo row is on screen")
}

fn wait_for(
    receiver: &mpsc::Receiver<Vec<u8>>,
    transcript: &mut Vec<u8>,
    from: usize,
    needle: &[u8],
    timeout: Duration,
) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if transcript[from.min(transcript.len())..]
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

/// Collect whatever the client emits for `duration`, so a later mark covers frames already sent.
fn drain_for(receiver: &mpsc::Receiver<Vec<u8>>, transcript: &mut Vec<u8>, duration: Duration) {
    let deadline = Instant::now() + duration;
    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        match receiver.recv_timeout(remaining.min(Duration::from_millis(100))) {
            Ok(chunk) => transcript.extend(chunk),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// Wait for an SGR that turns inverse video on past `from`.
fn wait_for_inverse(
    receiver: &mpsc::Receiver<Vec<u8>>,
    transcript: &mut Vec<u8>,
    from: usize,
    timeout: Duration,
) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if has_inverse_sgr(&transcript[from.min(transcript.len())..]) {
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

/// Whether any SGR sequence in `bytes` turns inverse video on.
fn has_inverse_sgr(bytes: &[u8]) -> bool {
    let mut index = 0;
    while index + 1 < bytes.len() {
        if bytes[index] != 0x1b || bytes[index + 1] != b'[' {
            index += 1;
            continue;
        }
        let start = index + 2;
        let mut end = start;
        while bytes
            .get(end)
            .is_some_and(|byte| !(0x40..=0x7e).contains(byte))
        {
            end += 1;
        }
        if bytes.get(end) == Some(&b'm') && sgr_sets_inverse(&bytes[start..end]) {
            return true;
        }
        index = end + 1;
    }
    false
}

/// Whether an SGR parameter list contains the inverse attribute.
///
/// The parameters are walked rather than searched for a `7`: an extended color such as
/// `38;5;7` carries its arguments as further parameters, so a white foreground would otherwise
/// read as inverse video.
fn sgr_sets_inverse(parameters: &[u8]) -> bool {
    let text = String::from_utf8_lossy(parameters);
    let parameters: Vec<&str> = text.split(';').collect();
    let mut index = 0;
    while index < parameters.len() {
        // A parameter's sub-parameters (`4:3`) belong to its leading number.
        match parameters[index].split(':').next().unwrap_or_default() {
            "7" => return true,
            "38" | "48" | "58" => {
                index += match parameters.get(index + 1).copied() {
                    Some("5") => 3,
                    Some("2") => 5,
                    _ => 1,
                };
            }
            _ => index += 1,
        }
    }
    false
}

fn wait_for_text(runtime: &Path, session: &str, pane: u64, text: &str) {
    let output = command(
        runtime,
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
        pane_text(runtime, session, pane)
    );
}

fn pane_text(runtime: &Path, session: &str, pane: u64) -> String {
    let output = command(
        runtime,
        session,
        &["get-text", "--pane-id", &pane.to_string()],
    );
    assert_success(&output);
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn command(runtime: &Path, session: &str, arguments: &[&str]) -> Output {
    common::vvmux_command(runtime)
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
