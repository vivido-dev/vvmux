//! Opt-in public-process check using the actual Python example and an isolated vvmux session.
#![cfg(windows)]

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc;
use std::time::{Duration, Instant};
use vivid_sdk::presenter::{
    MediaConfig, PresenterConfig, PresenterListener, SocketListener, VirtualVivid,
};
use vvmux_terminal::pty::PtyProcess;

struct Cleanup {
    name: String,
}
struct StopPty(vvmux_terminal::pty::PtyControl);
impl Drop for StopPty {
    fn drop(&mut self) {
        self.0.terminate_blocking();
    }
}
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = Command::new(env!("CARGO_BIN_EXE_vvmux"))
            .args(["kill-session", "-t", &self.name])
            .output();
    }
}

#[test]
#[ignore = "requires uv and the sibling vivid_ui Python environment; run explicitly for overlay acceptance"]
fn python_hello_world_draws_and_increments_inside_a_real_vvmux() {
    let listener = SocketListener::bind("tcp:127.0.0.1:0").unwrap();
    let endpoint = listener.endpoint();
    let host = VirtualVivid::start_configured_eventless(
        listener,
        PresenterConfig::terminal_with_overlay(MediaConfig::default()),
    )
    .unwrap();
    host.update_metrics(1, 110, 32, (8, 16));
    let secret = host.issue_pane_capability(1).unwrap();
    let environment = vec![
        ("VIVID_ENDPOINT_CONTROL".into(), endpoint.clone()),
        ("VIVID_ENDPOINT_INTERACTIVE".into(), endpoint.clone()),
        ("VIVID_ENDPOINT_BULK".into(), endpoint.clone()),
        ("VIVID_ENDPOINT_REALTIME".into(), endpoint),
        ("VIVID_ROOT_SECRET".into(), secret),
    ];
    let name = format!("overlay-python-{}", std::process::id());
    let _cleanup = Cleanup { name: name.clone() };
    let cwd = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("vivid_ui");
    let parts = PtyProcess::spawn_argv(
        env!("CARGO_BIN_EXE_vvmux").as_ref(),
        &["new", "-s", &name],
        &cwd,
        110,
        32,
        &environment,
    )
    .unwrap();
    let mut reader = parts.reader;
    let control = parts.control.clone();
    let _stop = StopPty(control.clone());
    let (sender, receiver) = mpsc::sync_channel(128);
    let read = std::thread::spawn(move || {
        let mut bytes = [0; 8192];
        while let Ok(length) = reader.read(&mut bytes) {
            if length == 0 || sender.send(bytes[..length].to_vec()).is_err() {
                break;
            }
        }
    });
    let mut transcript = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(20);
    while !transcript.windows(8).any(|bytes| bytes == b"\x1b[?1049h") {
        assert!(
            Instant::now() < deadline,
            "vvmux did not attach: {}",
            String::from_utf8_lossy(&transcript)
        );
        if let Ok(bytes) = receiver.recv_timeout(Duration::from_millis(50)) {
            if transcript.len() + bytes.len() > 256 * 1024 {
                transcript.clear();
            }
            transcript.extend(bytes);
        }
    }
    // Input stays in the real PTY: the pane shell launches the same command as the user.
    parts
        .input
        .send(b"uv run python .\\examples\\python\\hello_world.py --duration 30\r")
        .unwrap();
    let wait_label = |label: &str, transcript: &mut Vec<u8>| {
        let deadline = Instant::now() + Duration::from_secs(25);
        loop {
            let snapshot = host
                .prepare_projection_snapshot_with_viewports(&HashSet::from([1]), &HashMap::new());
            for source in &snapshot.sources {
                if let Some(body) = &source.retained
                    && let Ok(frame) = vivid_protocol::vector::Frame::decode(body)
                    && frame.canvas.commands().iter().any(|command| {
                        matches!(command,
                        vivid_protocol::vector::Command::Text(text) if text.text == label)
                    })
                {
                    return snapshot
                        .surfaces
                        .iter()
                        .find_map(|surface| surface.overlay_window)
                        .unwrap();
                }
            }
            assert!(
                Instant::now() < deadline,
                "missing {label}; terminal: {}",
                String::from_utf8_lossy(transcript)
            );
            if let Ok(bytes) = receiver.recv_timeout(Duration::from_millis(20))
                && transcript.len() < 256 * 1024
            {
                transcript.extend(bytes);
            }
        }
    };
    let window = wait_label("Clicked 0 times", &mut transcript);
    let (x, y) = (window.x as f64 + 35., window.y as f64 + 75.);
    assert!(host.overlay_pointer(1, x, y, Some((1, true)), 0).unwrap());
    assert!(host.overlay_pointer(1, x, y, Some((1, false)), 0).unwrap());
    wait_label("Clicked 1 times", &mut transcript);
    control.terminate_blocking();
    drop(receiver);
    read.join().unwrap();
}
