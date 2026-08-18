#![cfg(unix)]

use crate::common;

use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
fn focused_pane_osc52_store_reaches_the_attached_clients_outer_pty() {
    let executable = PathBuf::from(env!("CARGO_BIN_EXE_vvmux"));
    let directory = tempfile::Builder::new()
        .prefix("vvm-osc52-")
        .tempdir_in("/tmp")
        .unwrap();
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let runtime = directory.path().to_path_buf();
    let shell = directory.path().join("fixture-shell");
    fs::write(
        &shell,
        br#"#!/bin/sh
while IFS= read -r line; do
    [ "$line" = emit ] && printf '\033]52;c;aMOpbGxvIPCfpoA=\033\\'
done
"#,
    )
    .unwrap();
    fs::set_permissions(&shell, fs::Permissions::from_mode(0o700)).unwrap();
    let config = directory.path().join("vvmux.toml");
    fs::write(
        &config,
        format!(
            "[general]\nshell = \"{}\"\nrender_interval_ms = 1\nstatus_visible = false\n",
            shell.display()
        ),
    )
    .unwrap();

    let session = format!(
        "osc52-probe-{}-{}",
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
    let isolation = [
        (
            "XDG_RUNTIME_DIR".to_owned(),
            runtime.to_string_lossy().into_owned(),
        ),
        (
            "XDG_CONFIG_HOME".to_owned(),
            runtime.to_string_lossy().into_owned(),
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
    parts
        .input
        .send(
            format!(
                "exec {} --config {} new -s {session}\n",
                executable.display(),
                config.display()
            )
            .as_bytes(),
        )
        .unwrap();
    let mut transcript = Vec::new();
    assert!(
        wait_for(
            &receiver,
            &mut transcript,
            b"\x1b[?1049h",
            Duration::from_secs(15)
        ),
        "client did not attach: {}",
        String::from_utf8_lossy(&transcript)
    );

    parts.input.send(b"emit\r").unwrap();
    assert!(
        wait_for(
            &receiver,
            &mut transcript,
            b"\x1b]52;c;aMOpbGxvIPCfpoA=\x1b\\",
            Duration::from_secs(15)
        ),
        "the attached client never mirrored the decoded clipboard text"
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
