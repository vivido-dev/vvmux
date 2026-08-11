#![cfg(windows)]

use std::ffi::OsString;
use std::io::Read;
use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use vvmux_terminal::pty::PtyProcess;

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
