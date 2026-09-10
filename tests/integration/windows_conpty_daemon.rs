#![cfg(windows)]

use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use vvmux_terminal::pty::PtyProcess;
use vvmux_terminal::{Terminal, TerminalEvent};

const ANCHOR_PROBE_ENV: &str = "VVMUX_TEST_CONPTY_ANCHOR_CHILD";
const ANCHOR_BODY: &str =
    "VIVID;3;A;AAAAAAAAAAAAAAAAAAAAAA;0000000000000003;0000000000000007;AAAAAAAAAAAAAAAAAAAAAA";

/// Re-executed by the parent in an actual narrow ConPTY, with libtest capture disabled.
#[test]
fn anchor_probe_child() {
    if std::env::var_os(ANCHOR_PROBE_ENV).is_none() {
        return;
    }
    let mut stdout = std::io::stdout().lock();
    for _ in 0..24 {
        writeln!(stdout, "line").unwrap();
    }
    stdout.flush().unwrap();
    // Let ConPTY present the preceding text so the anchor is a bottom-row update, not
    // part of its initial full-screen paint (which preserves even narrow markers).
    std::thread::sleep(Duration::from_millis(100));
    write!(stdout, "{ANCHOR_BODY};VIVID-END").unwrap();
    stdout.flush().unwrap();
    std::thread::sleep(Duration::from_millis(100));
    write!(stdout, "\r\n\r\nANCHOR-PROBE-DONE").unwrap();
    stdout.flush().unwrap();
    // The parent terminates this private child after DONE, before libtest can print its
    // own trailing lines and obscure how far the producer's two reserved rows scrolled.
    std::thread::sleep(Duration::from_secs(5));
}

#[test]
fn anchors_survive_real_conpty_bottom_row_wrapping() {
    let executable = std::env::current_exe().unwrap();
    for width in [40, 80, 120] {
        let parts = PtyProcess::spawn_argv(
            executable.as_os_str(),
            &[
                "--exact",
                "windows_conpty_daemon::anchor_probe_child",
                "--nocapture",
            ],
            executable.parent().unwrap(),
            width,
            24,
            &[(ANCHOR_PROBE_ENV.into(), "1".into())],
        )
        .unwrap();
        let control = parts.control.clone();
        let mut reader = parts.reader;
        let (sender, receiver) = mpsc::channel();
        let reader_thread = std::thread::spawn(move || {
            let mut buffer = [0; 4096];
            while let Ok(read) = reader.read(&mut buffer) {
                if read == 0 || sender.send(buffer[..read].to_vec()).is_err() {
                    break;
                }
            }
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut terminal = Terminal::new(24, usize::from(width), 100);
        let mut output = Vec::new();
        let mut events = Vec::new();
        while Instant::now() < deadline {
            if let Ok(chunk) = receiver.recv_timeout(Duration::from_millis(100)) {
                events.extend(terminal.feed(&chunk));
                output.extend(chunk);
                if output
                    .windows(b"ANCHOR-PROBE-DONE".len())
                    .any(|part| part == b"ANCHOR-PROBE-DONE")
                {
                    break;
                }
            }
        }
        control.terminate_blocking();
        drop(receiver);
        reader_thread.join().unwrap();
        let anchors: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                TerminalEvent::VividMarker {
                    marker,
                    row,
                    column,
                    ..
                } => Some((marker.as_str(), *row, *column)),
                _ => None,
            })
            .collect();
        // The synthetic marker contains no session credentials. Do not capture real producer
        // output in this test: failure diagnostics below are deliberately synthetic only.
        assert_eq!(anchors, [(ANCHOR_BODY, 23, 0)], "width {width}: {output:?}");
        let text: String = terminal
            .cells()
            .iter()
            .flatten()
            .map(|cell| cell.ch)
            .collect();
        assert!(!text.contains("VIVID"));
        let anchor_event = events
            .iter()
            .position(|event| matches!(event, TerminalEvent::VividMarker { .. }))
            .unwrap();
        let scrolled: i32 = events[anchor_event + 1..]
            .iter()
            .filter_map(|event| match event {
                TerminalEvent::GridScroll { lines, .. } => Some(*lines),
                _ => None,
            })
            .sum();
        assert_eq!(
            scrolled, 2,
            "only reserved rows may scroll the anchor at width {width}"
        );
    }
}

#[test]
fn detached_server_reports_readiness_from_a_conpty_shell() {
    let executable = PathBuf::from(env!("CARGO_BIN_EXE_vvmux"));
    let cwd = executable.parent().unwrap();
    let shell = std::env::var_os("COMSPEC").unwrap_or_else(|| OsString::from("cmd.exe"));
    let session = format!(
        "conpty-readiness-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let parts = PtyProcess::spawn(&shell, None, cwd, 80, 24, &[]).unwrap();
    let control = parts.control.clone();
    let mut reader = parts.reader;
    let (sender, receiver) = mpsc::channel();
    let reader_thread = std::thread::spawn(move || {
        let mut buffer = [0_u8; 4096];
        while let Ok(read) = reader.read(&mut buffer) {
            if read == 0 || sender.send(buffer[..read].to_vec()).is_err() {
                break;
            }
        }
    });

    let quoted_executable = format!("\"{}\"", executable.display());
    let command = format!("{quoted_executable} new -d -s {session}\r\n");
    parts.input.send(command.as_bytes()).unwrap();

    let expected = format!("created vvmux session {session}");
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut output = Vec::new();
    while Instant::now() < deadline && !String::from_utf8_lossy(&output).contains(&expected) {
        if let Ok(chunk) = receiver.recv_timeout(Duration::from_millis(250)) {
            output.extend(chunk);
        }
    }

    let cleanup = Command::new(&executable)
        .args(["kill-session", "-t", &session])
        .output();
    control.terminate_blocking();
    drop(receiver);
    reader_thread.join().unwrap();

    assert!(
        String::from_utf8_lossy(&output).contains(&expected),
        "vvmux did not report readiness from ConPTY:\n{}",
        String::from_utf8_lossy(&output)
    );
    let cleanup = cleanup.unwrap();
    assert!(
        cleanup.status.success(),
        "could not clean up ConPTY test session: {}",
        String::from_utf8_lossy(&cleanup.stderr)
    );
}
