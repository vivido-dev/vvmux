#![cfg(unix)]

//! The right-click pane menu, driven through a real attached client with raw SGR mouse bytes.
//!
//! Proves the menu opens on a right click in a pane that has not asked for mouse reports, that the
//! opening click's own release does not choose anything, that the swap pair and the zoom item are
//! named after the pane's context, and that each item — chosen by key or by clicking it — does
//! what it says to the live layout.

use crate::common;

use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use vvmux_terminal::pty::{PtyInput, PtyProcess};

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

struct Client {
    input: PtyInput,
    receiver: mpsc::Receiver<Vec<u8>>,
    transcript: Vec<u8>,
}

impl Client {
    /// Right-click and release at a zero-based display cell, then wait for the menu to be drawn.
    fn open_menu(&mut self, x: u16, y: u16) -> String {
        let mark = self.transcript.len();
        let (column, row) = (x + 1, y + 1);
        self.input
            .send(format!("\x1b[<2;{column};{row}M\x1b[<2;{column};{row}m").as_bytes())
            .unwrap();
        assert!(
            self.wait_for(mark, b"Kill", Duration::from_secs(15)),
            "the pane menu was never drawn"
        );
        // Let the rest of the frame arrive so the labels below can be read from it.
        self.drain_for(Duration::from_millis(300));
        String::from_utf8_lossy(&self.transcript[mark..]).into_owned()
    }

    fn wait_for(&mut self, from: usize, needle: &[u8], timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self.transcript[from.min(self.transcript.len())..]
                .windows(needle.len())
                .any(|window| window == needle)
            {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            if let Ok(chunk) = self.receiver.recv_timeout(Duration::from_millis(100)) {
                self.transcript.extend(chunk);
            }
        }
    }

    fn drain_for(&mut self, duration: Duration) {
        let deadline = Instant::now() + duration;
        while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
            match self
                .receiver
                .recv_timeout(remaining.min(Duration::from_millis(50)))
            {
                Ok(chunk) => self.transcript.extend(chunk),
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
        }
    }
}

#[test]
fn right_click_pane_menu_splits_swaps_zooms_respawns_and_kills() {
    let executable = PathBuf::from(env!("CARGO_BIN_EXE_vvmux"));
    let directory = tempfile::Builder::new()
        .prefix("vvm-")
        .tempdir_in("/tmp")
        .unwrap();
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let runtime = directory.path().to_path_buf();
    // The fixture never enables mouse reporting, so a right click belongs to vvmux.
    let shell = directory.path().join("fixture-shell");
    fs::write(
        &shell,
        b"#!/bin/sh\nprintf 'READY pane=%s\\n' \"$VVMUX_PANE_ID\"\nexec cat >/dev/null\n",
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
        "menu-probe-{}-{}",
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
            runtime.to_str().unwrap().to_owned(),
        ),
        (
            "XDG_CONFIG_HOME".to_owned(),
            runtime.to_str().unwrap().to_owned(),
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
    let mut client = Client {
        input: parts.input,
        receiver,
        transcript: Vec::new(),
    };
    client
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
    assert!(
        client.wait_for(0, b"\x1b[?1049h", Duration::from_secs(15)),
        "the client never entered the alternate screen"
    );
    wait_for_text(&runtime, &session, 1, "READY pane=1");

    // A lone pane: splits, kill, respawn, and a zoom that has nothing to zoom over. No swap pair.
    let menu = client.open_menu(10, 5);
    assert!(menu.contains("Horizontal Split") && menu.contains("Vertical Split"));
    assert!(
        !menu.contains("Swap"),
        "a lone pane has no swap pair: {menu}"
    );
    assert_eq!(
        pane_ids(&runtime, &session),
        [1],
        "the opening release chose nothing"
    );
    client.input.send(b"h").unwrap();
    wait_for_panes(&runtime, &session, &[1, 2]);
    let layout = layout_now(&runtime, &session);
    assert!(
        pane_x(&layout, 1) < pane_x(&layout, 2),
        "h splits side by side"
    );

    // Side by side, the pair is Left/Right. Click "Swap Right": the menu's top-left is the click,
    // so its fifth entry row — after two splits, a rule, and Swap Left — is five rows below.
    let menu = client.open_menu(10, 5);
    assert!(menu.contains("Swap Left") && menu.contains("Swap Right"));
    assert!(!menu.contains("Swap Up") && !menu.contains("Swap Down"));
    client.input.send(b"\x1b[<0;13;11M\x1b[<0;13;11m").unwrap();
    wait_until(&runtime, &session, "pane 1 moved right", |layout| {
        pane_x(layout, 1) > pane_x(layout, 2)
    });

    // Pane 1 is now on the right. Zoom it, and the same menu offers Unzoom.
    let right = u16::try_from(pane_x(&layout_now(&runtime, &session), 1)).unwrap() + 10;
    let menu = client.open_menu(right, 5);
    assert!(menu.contains("Zoom") && !menu.contains("Unzoom"));
    client.input.send(b"z").unwrap();
    wait_until(&runtime, &session, "pane 1 zoomed", |layout| {
        layout["tabs"][0]["zoomed_pane_id"] == 1
    });
    let menu = client.open_menu(10, 5);
    assert!(
        menu.contains("Unzoom"),
        "a zoomed tab offers Unzoom: {menu}"
    );

    // Respawn replaces the process in the same slot: a new pane that keeps the zoom.
    client.input.send(b"R").unwrap();
    wait_for_panes(&runtime, &session, &[2, 3]);
    wait_for_text(&runtime, &session, 3, "READY pane=3");
    let layout = layout_now(&runtime, &session);
    assert_eq!(layout["tabs"][0]["zoomed_pane_id"], 3);

    // q closes the menu without acting; Kill then closes the pane.
    client.open_menu(10, 5);
    client.input.send(b"q").unwrap();
    client.drain_for(Duration::from_millis(300));
    assert_eq!(pane_ids(&runtime, &session), [2, 3]);
    client.open_menu(10, 5);
    client.input.send(b"X").unwrap();
    wait_for_panes(&runtime, &session, &[2]);

    drop(guard);
    control.terminate_blocking();
    drop(client);
    reader_thread.join().unwrap();
}

fn layout_now(runtime: &Path, session: &str) -> Value {
    let output = common::vvmux_command(runtime)
        .args(["msg", "--target", session, "layout"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "layout failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn pane_ids(runtime: &Path, session: &str) -> Vec<u64> {
    let mut ids: Vec<u64> = layout_now(runtime, session)["tabs"][0]["panes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|pane| pane["pane_id"].as_u64().unwrap())
        .collect();
    ids.sort_unstable();
    ids
}

fn pane_x(layout: &Value, pane_id: u64) -> u64 {
    layout["tabs"][0]["panes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|pane| pane["pane_id"] == pane_id)
        .and_then(|pane| pane["geometry"]["x"].as_u64())
        .unwrap_or_else(|| panic!("pane {pane_id} missing from {layout}"))
}

fn wait_until(runtime: &Path, session: &str, what: &str, done: impl Fn(&Value) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let layout = layout_now(runtime, session);
        if done(&layout) {
            return;
        }
        assert!(Instant::now() < deadline, "{what} never happened: {layout}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn wait_for_panes(runtime: &Path, session: &str, expected: &[u64]) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let ids = pane_ids(runtime, session);
        if ids == expected {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "panes stayed {ids:?}, expected {expected:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn wait_for_text(runtime: &Path, session: &str, pane: u64, text: &str) {
    let output = common::vvmux_command(runtime)
        .args([
            "msg",
            "--target",
            session,
            "wait",
            "text",
            text,
            "--pane-id",
            &pane.to_string(),
            "--timeout",
            "15s",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "pane {pane} never showed {text:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
