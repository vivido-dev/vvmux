#![cfg(windows)]

use std::io::Read;
use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use vvmux_terminal::pty::PtyProcess;

#[test]
fn detach_then_immediate_reattach_repaints_the_session() {
    let executable = PathBuf::from(env!("CARGO_BIN_EXE_vvmux"));
    let cwd = executable.parent().unwrap();
    let session = format!(
        "reattach-probe-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let marker = format!("VVMUX_REATTACH_{}", std::process::id());

    let shell = std::env::var_os("COMSPEC").unwrap_or_else(|| "cmd.exe".into());
    let parts = PtyProcess::spawn(&shell, cwd, 100, 30, &[]).unwrap();
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
    assert!(wait_for_after(
        &receiver,
        &mut transcript,
        b">",
        0,
        Duration::from_secs(10)
    ));
    parts
        .input
        .send(format!("\"{}\" new -s {session}\r\n", executable.display()).as_bytes())
        .unwrap();
    assert!(wait_for_after(
        &receiver,
        &mut transcript,
        b"\x1b[?1049h",
        0,
        Duration::from_secs(15)
    ));

    parts
        .input
        .send(format!("echo {marker}\r\n").as_bytes())
        .unwrap();
    assert!(wait_for_occurrences(
        &receiver,
        &mut transcript,
        marker.as_bytes(),
        2,
        Duration::from_secs(5)
    ));

    let detach_start = transcript.len();
    parts.input.send(b"\x02d").unwrap();
    assert!(wait_for_after(
        &receiver,
        &mut transcript,
        b"\x1b[?1049l",
        detach_start,
        Duration::from_secs(5)
    ));

    let reattach_start = transcript.len();
    parts
        .input
        .send(format!("\"{}\" attach -t {session}\r\n", executable.display()).as_bytes())
        .unwrap();
    assert!(wait_for_after(
        &receiver,
        &mut transcript,
        b"\x1b[?1049h",
        reattach_start,
        Duration::from_secs(10)
    ));
    assert!(
        wait_for_after(
            &receiver,
            &mut transcript,
            marker.as_bytes(),
            reattach_start,
            Duration::from_secs(5)
        ),
        "reattached session did not repaint its retained terminal contents:\n{}",
        String::from_utf8_lossy(&transcript[reattach_start..])
    );

    let _ = Command::new(&executable)
        .args(["kill-session", "-t", &session])
        .output();
    control.terminate_blocking();
    drop(receiver);
    reader_thread.join().unwrap();
}

fn wait_for_after(
    receiver: &mpsc::Receiver<Vec<u8>>,
    transcript: &mut Vec<u8>,
    needle: &[u8],
    offset: usize,
    timeout: Duration,
) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if transcript[offset..]
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

fn wait_for_occurrences(
    receiver: &mpsc::Receiver<Vec<u8>>,
    transcript: &mut Vec<u8>,
    needle: &[u8],
    expected: usize,
    timeout: Duration,
) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if transcript
            .windows(needle.len())
            .filter(|window| *window == needle)
            .count()
            >= expected
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
