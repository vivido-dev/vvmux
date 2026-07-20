#![cfg(windows)]

use std::io::Read;
use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use vvmux_terminal::pty::PtyProcess;

#[test]
fn ctrl_c_interrupts_pane_process_through_attached_session() {
    let executable = PathBuf::from(env!("CARGO_BIN_EXE_vvmux"));
    let cwd = executable.parent().unwrap().to_path_buf();
    let session = format!(
        "ctrlprobe-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );

    let shell = std::env::var_os("COMSPEC").unwrap_or_else(|| "cmd.exe".into());
    let parts = PtyProcess::spawn(&shell, &cwd, 100, 30, &[]).unwrap();
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
    let wait_for = |needle: &str, timeout_ms: u64, transcript: &mut Vec<u8>| -> bool {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            if String::from_utf8_lossy(transcript).contains(needle) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            if let Ok(chunk) = receiver.recv_timeout(Duration::from_millis(200)) {
                transcript.extend(chunk);
            }
        }
    };

    let host_ready = wait_for(">", 10_000, &mut transcript);
    println!("host shell ready: {host_ready}");
    let attach = format!("\"{}\" new -s {session}\r\n", executable.display());
    parts.input.send(attach.as_bytes()).unwrap();
    // The alternate-screen enter marks the client attaching; the pane prompt follows.
    let attached = wait_for("\x1b[?1049h", 15_000, &mut transcript);
    println!("attached: {attached}");
    std::thread::sleep(Duration::from_millis(1500));
    while let Ok(chunk) = receiver.try_recv() {
        transcript.extend(chunk);
    }

    parts.input.send(b"ping -n 30 127.0.0.1\r\n").unwrap();
    let pinging = wait_for("TTL=128", 10_000, &mut transcript);
    println!("ping running: {pinging}");

    let before_interrupt = Instant::now();
    parts.input.send(b"\x03").unwrap();
    // An interrupted ping prints statistics and Control-C, and the prompt returns quickly.
    let interrupted = wait_for("Control-C", 5_000, &mut transcript);
    println!(
        "interrupted: {interrupted} after {:?}",
        before_interrupt.elapsed()
    );

    let _ = Command::new(&executable)
        .args(["kill-session", "-t", &session])
        .output();
    control.terminate_blocking();
    drop(receiver);
    reader_thread.join().unwrap();

    let text = String::from_utf8_lossy(&transcript);
    println!(
        "=== transcript tail ===\n{}",
        &text[text.len().saturating_sub(2000)..]
    );
    assert!(attached && pinging, "session did not reach a running ping");
    assert!(interrupted, "Ctrl+C did not interrupt the pane process");
}
