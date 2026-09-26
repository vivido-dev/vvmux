#![cfg(unix)]

//! The sidebar tab view, driven through a real attached client with raw SGR mouse bytes.
//!
//! Proves the sidebar takes columns instead of a row, that a multi-pane tab's `+` expands it into
//! its named panes, that clicking a pane switches to its tab and focuses it, that clicking a tab
//! switches to it, and that `C-b T` moves the sidebar to the other side.

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
    /// Left-click and release at a zero-based display cell.
    fn click(&mut self, x: u16, y: u16) {
        let (column, row) = (x + 1, y + 1);
        self.input
            .send(format!("\x1b[<0;{column};{row}M\x1b[<0;{column};{row}m").as_bytes())
            .unwrap();
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
fn sidebar_expands_tabs_and_clicks_select_tabs_and_panes() {
    let executable = PathBuf::from(env!("CARGO_BIN_EXE_vvmux"));
    let directory = tempfile::Builder::new()
        .prefix("vvs-")
        .tempdir_in("/tmp")
        .unwrap();
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let runtime = directory.path().to_path_buf();
    // The fixture never enables mouse reporting, so every click belongs to vvmux.
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
            "[general]\nshell = \"{}\"\nrender_interval_ms = 1\ntab_view = \"left\"\n",
            shell.display()
        ),
    )
    .unwrap();
    let session = format!(
        "sidebar-probe-{}-{}",
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
    // 100 columns give a 24-column sidebar: text in columns 0..23, the separator in column 23.
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

    let layout = layout_now(&runtime, &session);
    assert_eq!(
        layout["area"]["x"], 24,
        "the left sidebar pushes panes right"
    );
    assert_eq!(layout["area"]["height"], 30, "a sidebar takes no row");

    // Tab 1 holds panes 1 (named) and 2; tab 2 holds pane 3 and is active.
    msg(
        &runtime,
        &session,
        &["split", "horizontal", "--pane-id", "1"],
    );
    msg(
        &runtime,
        &session,
        &["pane-rename", "--pane-id", "1", "--name", "editor"],
    );
    msg(&runtime, &session, &["new-tab", "--name", "logs"]);
    wait_for_text(&runtime, &session, 3, "READY pane=3");
    msg(&runtime, &session, &["focus", "--pane-id", "3"]);
    let tab_ids: Vec<u64> = layout_now(&runtime, &session)["tabs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tab| tab["tab_id"].as_u64().unwrap())
        .collect();
    assert_eq!(tab_ids.len(), 2);
    wait_until(&runtime, &session, "tab 2 active", |layout| {
        layout["active_tab_id"] == tab_ids[1]
    });

    // Let every frame from before the switch arrive, so the next one reflects only the click.
    assert!(
        client.wait_for(0, b"2 logs", Duration::from_secs(15)),
        "the sidebar never listed the named tab"
    );
    client.drain_for(Duration::from_millis(500));

    // Row 0 is tab 1; its marker is the first cell.
    let mark = client.transcript.len();
    client.click(0, 0);
    assert!(
        client.wait_for(mark, b"editor", Duration::from_secs(15)),
        "expanding the tab never listed its named pane"
    );
    assert_eq!(
        layout_now(&runtime, &session)["active_tab_id"],
        tab_ids[1],
        "the marker expands without switching tabs"
    );

    // Rows: 0 tab 1, 1 pane 1 ("editor"), 2 pane 2, 3 tab 2.
    client.click(6, 2);
    wait_until(&runtime, &session, "pane 2 focused in tab 1", |layout| {
        layout["active_tab_id"] == tab_ids[0] && layout["tabs"][0]["focused_pane_id"] == 2
    });

    client.click(4, 3);
    wait_until(&runtime, &session, "tab 2 selected by click", |layout| {
        layout["active_tab_id"] == tab_ids[1]
    });

    // C-b T moves from left to right: the panes start at column 0 and keep their width.
    client.input.send(b"\x02T").unwrap();
    wait_until(&runtime, &session, "the right sidebar", |layout| {
        layout["area"]["x"] == 0 && layout["area"]["width"] == 76
    });
    client.input.send(b"\x02T").unwrap();
    wait_until(&runtime, &session, "the hidden tab list", |layout| {
        layout["area"]["width"] == 100 && layout["area"]["height"] == 30
    });

    drop(guard);
    control.terminate_blocking();
    drop(client);
    reader_thread.join().unwrap();
}

fn msg(runtime: &Path, session: &str, args: &[&str]) {
    let output = common::vvmux_command(runtime)
        .args(["msg", "--target", session])
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "msg {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
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
