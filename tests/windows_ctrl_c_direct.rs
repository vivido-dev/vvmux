#![cfg(windows)]

use std::io::Read;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use vvmux_terminal::pty::PtyProcess;

#[test]
fn ctrl_c_interrupts_conpty_child_process() {
    let shell = std::env::var_os("COMSPEC").unwrap_or_else(|| "cmd.exe".into());
    let parts = PtyProcess::spawn(&shell, &PathBuf::from("C:\\"), 100, 30, &[]).unwrap();
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

    assert!(wait_for(">", 10_000, &mut transcript), "no shell prompt");
    parts.input.send(b"ping -n 30 127.0.0.1\r\n").unwrap();
    assert!(
        wait_for("TTL=128", 10_000, &mut transcript),
        "ping did not start"
    );
    let started = Instant::now();
    parts.input.send(b"\x03").unwrap();
    let interrupted = wait_for("Control-C", 5_000, &mut transcript);
    println!(
        "direct ConPTY interrupted: {interrupted} after {:?}",
        started.elapsed()
    );
    control.terminate_blocking();
    drop(receiver);
    reader_thread.join().unwrap();
    assert!(interrupted, "Ctrl+C did not interrupt through direct ConPTY");
}
