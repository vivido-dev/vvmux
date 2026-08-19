#![cfg(unix)]

use crate::common;

use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

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
fn direct_attach_routes_input_to_one_pane_and_detaches_with_prefix_q() {
    let directory = tempfile::Builder::new()
        .prefix("vvdirect-")
        .tempdir_in("/tmp")
        .unwrap();
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let runtime = directory.path().to_path_buf();
    let shell = directory.path().join("fixture-shell");
    fs::write(
        &shell,
        br#"#!/bin/sh
printf 'READY pane=%s\n' "$VVMUX_PANE_ID"
while IFS= read -r line; do printf 'OUT pane=%s:%s\n' "$VVMUX_PANE_ID" "$line"; done
"#,
    )
    .unwrap();
    fs::set_permissions(&shell, fs::Permissions::from_mode(0o700)).unwrap();
    let config = directory.path().join("config.toml");
    fs::write(
        &config,
        format!(
            "[general]\nshell = {:?}\nrender_interval_ms = 1\nstatus_visible = true\n",
            shell.to_str().unwrap()
        ),
    )
    .unwrap();
    let session = format!("direct-{}", std::process::id());
    let guard = SessionGuard {
        runtime: runtime.clone(),
        name: session.clone(),
    };

    assert!(
        common::vvmux_command(&runtime)
            .args([
                "--config",
                config.to_str().unwrap(),
                "new",
                "-s",
                &session,
                "-d",
            ])
            .status()
            .unwrap()
            .success()
    );
    assert!(
        common::vvmux_command(&runtime)
            .args([
                "--config",
                config.to_str().unwrap(),
                "msg",
                "-t",
                &session,
                "split",
                "vertical",
                "--pane-id",
                "1",
            ])
            .status()
            .unwrap()
            .success()
    );

    let environment = [
        ("XDG_RUNTIME_DIR".to_owned(), runtime.display().to_string()),
        ("XDG_CONFIG_HOME".to_owned(), runtime.display().to_string()),
        ("TERM".to_owned(), "xterm-256color".to_owned()),
    ];
    let parts = PtyProcess::spawn(
        std::ffi::OsStr::new("/bin/sh"),
        None,
        directory.path(),
        90,
        20,
        &environment,
    )
    .unwrap();
    let mut reader = parts.reader;
    let (sender, receiver) = mpsc::channel();
    let reader_thread = std::thread::spawn(move || {
        let mut bytes = [0; 8192];
        while let Ok(count) = reader.read(&mut bytes) {
            if count == 0 || sender.send(bytes[..count].to_vec()).is_err() {
                break;
            }
        }
    });
    parts
        .input
        .send(
            format!(
                "exec {} --config {} attach -t {} --pane-id 2\n",
                env!("CARGO_BIN_EXE_vvmux"),
                config.display(),
                session
            )
            .as_bytes(),
        )
        .unwrap();
    let mut transcript = Vec::new();
    assert!(wait_for(&receiver, &mut transcript, b"READY pane=2"));
    parts.input.send(b"direct-only\n").unwrap();
    assert!(wait_for_pane_text(
        &runtime,
        &config,
        &session,
        2,
        "OUT pane=2:direct-only"
    ));
    assert!(!pane_text(&runtime, &config, &session, 1).contains("direct-only"));

    parts.input.send(b"\x02q").unwrap();
    assert!(wait_for(&receiver, &mut transcript, b"\x1b[?1049l"));
    drop(parts.input);
    drop(receiver);
    let _ = reader_thread.join();
    drop(guard);
}

fn pane_text(
    runtime: &std::path::Path,
    config: &std::path::Path,
    session: &str,
    pane: u64,
) -> String {
    let output = common::vvmux_command(runtime)
        .args([
            "--config",
            config.to_str().unwrap(),
            "msg",
            "-t",
            session,
            "get-text",
            "--source",
            "recent",
            "--pane-id",
            &pane.to_string(),
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn wait_for_pane_text(
    runtime: &std::path::Path,
    config: &std::path::Path,
    session: &str,
    pane: u64,
    needle: &str,
) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if pane_text(runtime, config, session, pane).contains(needle) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

fn wait_for(receiver: &mpsc::Receiver<Vec<u8>>, transcript: &mut Vec<u8>, needle: &[u8]) -> bool {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if transcript
            .windows(needle.len())
            .any(|window| window == needle)
        {
            return true;
        }
        match receiver.recv_timeout(Duration::from_millis(100)) {
            Ok(bytes) => transcript.extend(bytes),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    transcript
        .windows(needle.len())
        .any(|window| window == needle)
}
