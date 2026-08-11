#![cfg(windows)]

use std::io::Read;
use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use vvmux_terminal::pty::PtyProcess;

fn normalized(transcript: &[u8]) -> String {
    // vvmux renders runs of spaces as ESC[<n>C; normalize them to single spaces.
    let text = String::from_utf8_lossy(transcript).into_owned();
    let mut result = String::new();
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' && chars.peek() == Some(&'[') {
            let mut sequence = String::new();
            chars.next();
            while let Some(&next) = chars.peek() {
                chars.next();
                if next.is_ascii_alphabetic() {
                    sequence.push(next);
                    break;
                }
                sequence.push(next);
            }
            if sequence.ends_with('C') {
                result.push(' ');
            }
            continue;
        }
        result.push(ch);
    }
    result
}

#[test]
fn probe_first_image_after_cls() {
    let executable = PathBuf::from(env!("CARGO_BIN_EXE_vvmux"));
    let vivi = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("vivi/target/debug/vivi.exe");
    let test_png = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("vivi/test.png");
    assert!(vivi.exists() && test_png.exists());
    let session = format!(
        "imgprobe-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );

    let shell = std::env::var_os("COMSPEC").unwrap_or_else(|| "cmd.exe".into());
    let parts = PtyProcess::spawn(&shell, None, &PathBuf::from("C:\\"), 110, 32, &[]).unwrap();
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
    let wait_quiet = |transcript: &mut Vec<u8>, quiet_ms: u64| {
        while let Ok(chunk) = receiver.recv_timeout(Duration::from_millis(quiet_ms)) {
            transcript.extend(chunk);
        }
    };
    let wait_for = |needle: &str, timeout_ms: u64, transcript: &mut Vec<u8>| -> bool {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            if String::from_utf8_lossy(transcript).contains(needle)
                || normalized(transcript).contains(needle)
            {
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

    assert!(wait_for(">", 10_000, &mut transcript));
    let attach = format!("\"{}\" new -s {session}\r\n", executable.display());
    parts.input.send(attach.as_bytes()).unwrap();
    assert!(wait_for("\u{1b}[?1049h", 15_000, &mut transcript));
    wait_quiet(&mut transcript, 1500);

    parts
        .input
        .send(b"set PATH=F:\\Programs\\vcpkg\\installed\\x64-windows\\bin;%PATH%\r\n")
        .unwrap();
    wait_quiet(&mut transcript, 800);
    let run_vivi = format!("\"{}\" -v \"{}\"\r\n", vivi.display(), test_png.display());

    for (label, do_cls) in [
        ("run-1 (fresh)", false),
        ("run-2", false),
        ("run-3 (after cls)", true),
    ] {
        if do_cls {
            parts.input.send(b"cls\r\n").unwrap();
            wait_quiet(&mut transcript, 1200);
        }
        transcript.clear();
        parts.input.send(run_vivi.as_bytes()).unwrap();
        let done = wait_for("cells", 15_000, &mut transcript);
        wait_quiet(&mut transcript, 2500);
        let text = normalized(&transcript);
        println!("=== {label}: submitted={done} ===\n{text}\n=== end {label} ===\n");
    }

    let _ = Command::new(&executable)
        .args(["kill-session", "-t", &session])
        .output();
    control.terminate_blocking();
    drop(receiver);
    reader_thread.join().unwrap();
}
