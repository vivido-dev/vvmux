#![cfg(unix)]

//! A window the user can drag small and drag back open must not cost them a pane.
//!
//! Squeezing a three-pane tab past the point where the tiled panes still have room used to hand
//! the PTY a zero dimension, and a refused resize closed the pane and killed its program. The
//! panes disappeared for good: growing the window back could not restore what had been reaped.

use crate::common;

use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use vvmux_terminal::pty::PtyProcess;

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
fn panes_squeezed_by_a_shrinking_window_survive_and_come_back() {
    let executable = PathBuf::from(env!("CARGO_BIN_EXE_vvmux"));
    // Short `/tmp` root: the runtime directory holds the session socket, whose path must stay
    // inside the platform's `sun_path` limit. Isolating `XDG_CONFIG_HOME` and `XDG_RUNTIME_DIR`
    // keeps a developer's own `startup.toml` and live sessions out of this test's session.
    let directory = tempfile::Builder::new()
        .prefix("vvz-")
        .tempdir_in("/tmp")
        .unwrap();
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let runtime = directory.path().to_path_buf();
    let shell = directory.path().join("fixture-shell");
    fs::write(
        &shell,
        br#"#!/bin/sh
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
        "squeeze-probe-{}-{}",
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

    // The client only learns a display size from its own terminal, so the session is hosted in a
    // PTY the test can resize.
    // The hosted client owns the session, so it needs the same isolated runtime and config
    // directories the `msg` commands use.
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
            b"\x1b[?1049h",
            Duration::from_secs(15)
        ),
        "the client never entered the alternate screen"
    );

    // One pane on the left, two stacked on the right.
    assert_success(&command(
        &runtime,
        &session,
        &["split", "vertical", "--pane-id", "1"],
    ));
    assert_success(&command(
        &runtime,
        &session,
        &["split", "horizontal", "--pane-id", "2"],
    ));
    assert_eq!(
        wait_for_panes(&runtime, &session, 3),
        vec![1, 2, 3],
        "the three panes were not established"
    );

    // Small enough that the stacked panes have no room left for content at all.
    control.resize(16, 5).unwrap();
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(
        wait_for_panes(&runtime, &session, 3),
        vec![1, 2, 3],
        "a window too small to show the panes closed them"
    );

    control.resize(100, 30).unwrap();
    let restored = wait_for_panes(&runtime, &session, 3);
    assert_eq!(restored, vec![1, 2, 3], "the panes did not come back");
    for pane in restored {
        let geometry = wait_for_content(&runtime, &session, pane);
        assert!(
            geometry.0 > 1 && geometry.1 > 1,
            "pane {pane} stayed collapsed at {geometry:?} after the window grew back"
        );
    }

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

fn wait_for_panes(runtime: &Path, session: &str, expected: usize) -> Vec<u64> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut last = Vec::new();
    loop {
        let output = command(runtime, session, &["list-panes"]);
        if output.status.success() {
            let value: Value = serde_json::from_slice(&output.stdout).unwrap();
            last = value["panes"]
                .as_array()
                .unwrap()
                .iter()
                .map(|pane| pane["pane_id"].as_u64().unwrap())
                .collect();
            if last.len() == expected {
                return last;
            }
        }
        if Instant::now() >= deadline {
            return last;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn wait_for_content(runtime: &Path, session: &str, pane: u64) -> (u64, u64) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let geometry = content_geometry(runtime, session, pane);
        if (geometry.0 > 1 && geometry.1 > 1) || Instant::now() >= deadline {
            return geometry;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn content_geometry(runtime: &Path, session: &str, pane: u64) -> (u64, u64) {
    let output = command(
        runtime,
        session,
        &["inspect", "--pane-id", &pane.to_string()],
    );
    assert_success(&output);
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    let geometry = &value["pane"]["content_geometry"];
    (
        geometry["width"].as_u64().unwrap(),
        geometry["height"].as_u64().unwrap(),
    )
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
