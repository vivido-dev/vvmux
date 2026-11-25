//! Public-process overlay acceptance with a Cargo-built producer in an isolated vvmux pane.
#![cfg(windows)]

// Rust guideline compliant 2026-10-07
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::process::Command;
use std::sync::mpsc;
use std::time::{Duration, Instant};
use vivid_sdk::overlay::{Brush, Canvas, Color, Event, HitRole, Path, Point, Rect, Scalar, Text};
use vivid_sdk::presenter::{
    MediaConfig, PresenterConfig, PresenterListener, SocketListener, VirtualVivid,
};
use vivid_sdk::{OverlayLaneEvent, OverlaySession, OverlayWindowOptions};
use vvmux_terminal::pty::{PtyInput, PtyProcess};
use vvmux_terminal::{Terminal, TerminalEvent};

const CHILD_ENV: &str = "VVMUX_TEST_OVERLAY_COUNTER";

/// Re-execute this Cargo-built test binary inside the pane, as other process fixtures do.
#[test]
fn counter_producer_child() {
    if std::env::var_os(CHILD_ENV).is_none() {
        return;
    }
    let session = OverlaySession::from_env().unwrap();
    let bounds = Rect::new(0., 0., 240., 120.).unwrap();
    let window = session
        .create_window(OverlayWindowOptions::new(
            Rect::new(40., 40., 240., 120.).unwrap(),
            vivid_sdk::overlay::WindowMode::Floating,
        ))
        .unwrap();
    let present = |clicks| {
        let mut canvas = Canvas::new();
        let face = Path::rectangle(bounds).unwrap();
        canvas
            .fill(face.clone(), Brush::Solid(Color(0x2030_50ff)))
            .unwrap();
        canvas
            .push(vivid_sdk::overlay::Command::Text(Text {
                text: format!("Clicked {clicks} times"),
                origin: Point::new(20., 24.).unwrap(),
                size: Scalar::new(18.).unwrap(),
                family: String::new(),
                weight: 400,
                italic: false,
                color: Color(0xffff_ffff),
                max_width: None,
            }))
            .unwrap();
        canvas
            .push(vivid_sdk::overlay::Command::Hit {
                id: 1,
                path: face,
                role: HitRole::Input,
                cursor: None,
            })
            .unwrap();
        window.present(canvas).unwrap();
    };
    let mut clicks = 0;
    present(clicks);
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        match session.wait_event(Duration::from_millis(100)).unwrap() {
            Some(OverlayLaneEvent::Input(input))
                if session.event_targets(&input, &window).unwrap()
                    && matches!(
                        input.event,
                        Event::Pointer {
                            region: 1,
                            button: Some((1, true)),
                            ..
                        }
                    ) =>
            {
                clicks += 1;
                present(clicks);
            }
            Some(OverlayLaneEvent::ConnectionLost { diagnostic }) => panic!("{diagnostic}"),
            _ => {}
        }
    }
    window.close().unwrap();
    session.close().unwrap();
}

fn receive_output(
    receiver: &mpsc::Receiver<Vec<u8>>,
    input: &PtyInput,
    terminal: &mut Terminal,
    transcript: &mut Vec<u8>,
) {
    if let Ok(bytes) = receiver.recv_timeout(Duration::from_millis(20)) {
        // Process split terminal queries through the real parser, including ConPTY's startup CPR.
        for event in terminal.feed(&bytes) {
            if let TerminalEvent::PtyWrite(reply) = event {
                input.send(&reply).unwrap();
            }
        }
        if transcript.len() + bytes.len() > 256 * 1024 {
            transcript.clear();
        }
        transcript.extend(bytes);
    }
}

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
#[ignore = "requires real ConPTY processes; run explicitly for overlay acceptance"]
fn counter_draws_and_increments_inside_a_real_vvmux() {
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
    let directory = tempfile::tempdir().unwrap();
    let name = format!(
        "overlay-counter-{}-{}",
        std::process::id(),
        directory.path().file_name().unwrap().to_string_lossy()
    );
    let _cleanup = Cleanup { name: name.clone() };
    let config = directory.path().join("vvmux.toml");
    std::fs::write(&config, "[general]\nshell = 'cmd.exe'\n[plugins]\nenabled = false\n[session]\nauto_snapshot = false\n").unwrap();
    let parts = PtyProcess::spawn_argv(
        env!("CARGO_BIN_EXE_vvmux").as_ref(),
        &["--config", config.to_str().unwrap(), "new", "-s", &name],
        directory.path(),
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
    let mut terminal = Terminal::new(32, 110, 100);
    let deadline = Instant::now() + Duration::from_secs(20);
    while !transcript.windows(8).any(|bytes| bytes == b"\x1b[?1049h") {
        assert!(
            Instant::now() < deadline,
            "vvmux did not attach: {}",
            String::from_utf8_lossy(&transcript)
        );
        receive_output(&receiver, &parts.input, &mut terminal, &mut transcript);
    }
    // Only launch instructions use the PTY; the SDK sends all overlay media out of band.
    let executable = std::env::current_exe().unwrap();
    parts
        .input
        .send(format!("set \"{CHILD_ENV}=1\" && \"{}\" --exact overlay_counter::counter_producer_child --nocapture\r", executable.display()).as_bytes())
        .unwrap();
    let mut wait_label = |label: &str, transcript: &mut Vec<u8>| {
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
            receive_output(&receiver, &parts.input, &mut terminal, transcript);
        }
    };
    let window = wait_label("Clicked 0 times", &mut transcript);
    let (x, y) = (
        f64::from(i32::try_from(window.x).unwrap()) + 35.,
        f64::from(i32::try_from(window.y).unwrap()) + 75.,
    );
    assert!(host.overlay_pointer(1, x, y, Some((1, true)), 0).unwrap());
    assert!(host.overlay_pointer(1, x, y, Some((1, false)), 0).unwrap());
    wait_label("Clicked 1 times", &mut transcript);
    control.terminate_blocking();
    drop(receiver);
    read.join().unwrap();
}
