#![cfg(windows)]

use std::io::Read;
use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use vvmux_terminal::{Terminal, pty::PtyProcess};

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
    let vivid_environment = [
        "VIVID_ENDPOINT_CONTROL",
        "VIVID_ENDPOINT_REALTIME",
        "VIVID_ENDPOINT_BULK",
        "VIVID_ROOT_SECRET",
    ]
    .into_iter()
    .filter_map(|name| std::env::var(name).ok().map(|value| (name.into(), value)))
    .collect::<Vec<_>>();
    let vivid_enabled = vivid_environment
        .iter()
        .any(|(name, _)| name == "VIVID_ENDPOINT_CONTROL");

    let shell = std::env::var_os("COMSPEC").unwrap_or_else(|| "cmd.exe".into());
    let parts = PtyProcess::spawn(&shell, cwd, 100, 30, &vivid_environment).unwrap();
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
    assert!(
        wait_for_after(
            &receiver,
            &mut transcript,
            b"\x1b[?1049h",
            0,
            Duration::from_secs(15)
        ),
        "initial attach did not enter the alternate screen:\n{}",
        String::from_utf8_lossy(&transcript)
    );

    let second_tab_start = transcript.len();
    parts.input.send(b"\x02c").unwrap();
    assert!(wait_for_after(
        &receiver,
        &mut transcript,
        b">",
        second_tab_start,
        Duration::from_secs(10)
    ));
    let first_tab_return = transcript.len();
    parts.input.send(b"\x02p").unwrap();
    assert!(wait_for_after(
        &receiver,
        &mut transcript,
        b">",
        first_tab_return,
        Duration::from_secs(5)
    ));

    for frame in 0..6 {
        let frame_start = transcript.len();
        let text = format!("VVMUX_FRAME_{frame}");
        parts
            .input
            .send(format!("echo {text}\r\n").as_bytes())
            .unwrap();
        assert!(
            wait_for_after(
                &receiver,
                &mut transcript,
                text.as_bytes(),
                frame_start,
                Duration::from_secs(5)
            ),
            "frame {frame} was not rendered"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
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
    if vivid_enabled {
        let media_start = transcript.len();
        let repository = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf();
        let vivi = repository.join("vivi/target/release/vivi.exe");
        let image = repository.join("medias/lenna.png");
        parts
            .input
            .send(
                format!(
                    "\"{}\" --no-wait --zoom 0.1 \"{}\"\r\n",
                    vivi.display(),
                    image.display()
                )
                .as_bytes(),
            )
            .unwrap();
        assert!(wait_for_after(
            &receiver,
            &mut transcript,
            b">",
            media_start,
            Duration::from_secs(15)
        ));
    }

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
    while let Ok(chunk) = receiver.recv_timeout(Duration::from_millis(250)) {
        transcript.extend(chunk);
    }
    let mut rendered = Terminal::new(30, 100, 1000);
    rendered.feed(&transcript[reattach_start..]);
    assert!(
        rendered.visible_text(0).contains(&marker),
        "reattached terminal ended on a blank frame:\n{}",
        rendered.visible_text(0)
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
