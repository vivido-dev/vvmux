#![cfg(windows)]

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use vvmux_terminal::pty::PtyProcess;

#[test]
fn attached_split_panes_relayout_without_followup_input() {
    let executable = PathBuf::from(env!("CARGO_BIN_EXE_vvmux"));
    let cwd = executable.parent().unwrap();
    let session = format!(
        "resize-probe-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );

    let shell = std::env::var_os("COMSPEC").unwrap_or_else(|| "cmd.exe".into());
    let parts = PtyProcess::spawn(&shell, None, cwd, 100, 30, &[]).unwrap();
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

    let mut transcript = Vec::new();
    assert!(wait_for(
        &receiver,
        &mut transcript,
        b">",
        Duration::from_secs(10)
    ));
    let attach = format!("\"{}\" new -s {session}\r\n", executable.display());
    parts.input.send(attach.as_bytes()).unwrap();
    assert!(wait_for(
        &receiver,
        &mut transcript,
        b"\x1b[?1049h",
        Duration::from_secs(15)
    ));

    let split = command(
        &executable,
        &session,
        &["split", "vertical", "--pane-id", "1"],
    );
    assert_success(&split);
    let initial = wait_for_width(&executable, &session, 1, |width| width < 60)
        .expect("initial split layout was not published");

    control.resize(140, 30).unwrap();
    let resized = wait_for_width(&executable, &session, 1, |width| width > initial)
        .expect("split panes did not relayout until keyboard or mouse input");
    assert!(resized > initial, "pane width stayed at {initial}");

    let _ = Command::new(&executable)
        .args(["kill-session", "-t", &session])
        .output();
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

fn wait_for_width(
    executable: &Path,
    session: &str,
    pane_id: u64,
    predicate: impl Fn(u64) -> bool,
) -> Option<u64> {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let inspect = command(
            executable,
            session,
            &["inspect", "--pane-id", &pane_id.to_string()],
        );
        if inspect.status.success() {
            let value: Value = serde_json::from_slice(&inspect.stdout).unwrap();
            let width = value["pane"]["content_geometry"]["width"].as_u64().unwrap();
            if predicate(width) {
                return Some(width);
            }
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
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
