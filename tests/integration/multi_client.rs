//! Several clients attached to one session at once.
//!
//! Terminal frames reach every client; media reaches exactly one, the presenter. Each test drives
//! real `vvmux attach` processes in real PTYs, and the media test gives each client its own
//! in-process outer Vivid presenter so it can see which one a pane's image actually reaches.
#![cfg(unix)]

use crate::common;

use std::collections::HashSet;
use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine;
use serde_json::Value;
use vivid_sdk::presenter::{
    MediaConfig, PresenterConfig, PresenterListener, SocketListener, VirtualVivid,
};
use vvmux_terminal::Terminal;
use vvmux_terminal::pty::{PtyControl, PtyProcess};

const SHOWN: &str = "MULTI-CLIENT-IMAGE-SHOWN";

/// Runs only inside a vvmux pane: shows one image and keeps it on screen.
#[test]
#[ignore = "re-executed inside a pane by media_reaches_only_the_presenter_until_another_client_claims_it"]
fn image_producer_child() {
    if std::env::var_os("VIVID_ENDPOINT_CONTROL").is_none() {
        return;
    }
    let png = base64::engine::general_purpose::STANDARD
        .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=")
        .unwrap();
    let mut pane = vivid_sdk::PaneSession::from_env().unwrap();
    pane.show_encoded_image(&png).unwrap();
    println!("{SHOWN}");
    std::thread::sleep(Duration::from_secs(60));
    let _ = pane.close();
}

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

/// One `vvmux attach` running in its own PTY, with its output replayed into an emulator.
struct Client {
    input: vvmux_terminal::pty::PtyInput,
    control: PtyControl,
    receiver: mpsc::Receiver<Vec<u8>>,
    transcript: Vec<u8>,
    columns: u16,
    rows: u16,
}

impl Client {
    fn attach(
        directory: &Path,
        session: &str,
        columns: u16,
        rows: u16,
        arguments: &str,
        vivid: &[(String, String)],
    ) -> Self {
        let mut environment = vec![
            (
                "XDG_RUNTIME_DIR".to_owned(),
                directory.display().to_string(),
            ),
            (
                "XDG_CONFIG_HOME".to_owned(),
                directory.display().to_string(),
            ),
            (
                "XDG_STATE_HOME".to_owned(),
                directory.join("state").display().to_string(),
            ),
            ("HOME".to_owned(), directory.display().to_string()),
            ("TERM".to_owned(), "xterm-256color".to_owned()),
        ];
        environment.extend_from_slice(vivid);
        let parts = PtyProcess::spawn(
            std::ffi::OsStr::new("/bin/sh"),
            None,
            directory,
            columns,
            rows,
            &environment,
        )
        .unwrap();
        let mut reader = parts.reader;
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let mut bytes = [0; 8192];
            while let Ok(count) = reader.read(&mut bytes) {
                if count == 0 || sender.send(bytes[..count].to_vec()).is_err() {
                    break;
                }
            }
        });
        // A developer running the suite inside Vivido must not hand a text client their own
        // window's capability.
        let scrub = if vivid.is_empty() {
            "env -u VIVID_ENDPOINT_CONTROL -u VIVID_ROOT_SECRET "
        } else {
            ""
        };
        parts
            .input
            .send(
                format!(
                    "exec {scrub}{} attach -t {session} {arguments}\n",
                    env!("CARGO_BIN_EXE_vvmux"),
                )
                .as_bytes(),
            )
            .unwrap();
        Self {
            input: parts.input,
            control: parts.control,
            receiver,
            transcript: Vec::new(),
            columns,
            rows,
        }
    }

    fn send(&self, bytes: &[u8]) {
        self.input.send(bytes).unwrap();
    }

    /// Wait until this client's screen satisfies `reached`.
    fn wait_for(&mut self, what: &str, reached: impl Fn(&Terminal) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let mut screen = Terminal::new(usize::from(self.rows), usize::from(self.columns), 0);
            screen.feed(&self.transcript);
            if reached(&screen) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "client never showed {what}; screen:\n{}",
                screen.visible_text(0)
            );
            match self.receiver.recv_timeout(Duration::from_millis(50)) {
                Ok(bytes) => {
                    if self.transcript.len() > 4 * 1024 * 1024 {
                        self.transcript.drain(..2 * 1024 * 1024);
                    }
                    self.transcript.extend(bytes);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    panic!("client exited before it showed {what}")
                }
            }
        }
    }

    fn wait_for_text(&mut self, needle: &str) {
        self.wait_for(needle, |screen| screen.visible_text(0).contains(needle));
    }

    /// What this client shows now, after everything it has received so far.
    fn screen_text(&mut self) -> String {
        while let Ok(bytes) = self.receiver.try_recv() {
            self.transcript.extend(bytes);
        }
        let mut screen = Terminal::new(usize::from(self.rows), usize::from(self.columns), 0);
        screen.feed(&self.transcript);
        screen.visible_text(0)
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.control.terminate_blocking();
    }
}

fn temporary_runtime(prefix: &str) -> tempfile::TempDir {
    // Short root: the runtime directory holds the session socket, whose path must fit `sun_path`.
    let directory = tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in("/tmp")
        .unwrap();
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
    directory
}

fn unique(label: &str) -> String {
    format!(
        "{label}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )
}

fn message(runtime: &Path, config: &Path, session: &str, arguments: &[&str]) -> Output {
    common::vvmux_command(runtime)
        .args(["--config", config.to_str().unwrap(), "msg", "-t", session])
        .args(arguments)
        .output()
        .unwrap()
}

fn json(output: Output) -> Value {
    assert!(
        output.status.success(),
        "command failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn list_clients(runtime: &Path, config: &Path, session: &str) -> Value {
    json(message(runtime, config, session, &["list-clients"]))
}

/// Poll `list-clients` until `reached` holds, and return that listing.
fn wait_for_clients(
    runtime: &Path,
    config: &Path,
    session: &str,
    what: &str,
    reached: impl Fn(&Value) -> bool,
) -> Value {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let listing = list_clients(runtime, config, session);
        if reached(&listing) {
            return listing;
        }
        assert!(Instant::now() < deadline, "never saw {what}: {listing}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn client_ids(listing: &Value) -> Vec<u64> {
    listing["clients"]
        .as_array()
        .unwrap()
        .iter()
        .map(|client| client["client_id"].as_u64().unwrap())
        .collect()
}

fn pane_text(runtime: &Path, config: &Path, session: &str, pane: &str) -> String {
    let output = message(
        runtime,
        config,
        session,
        &["get-text", "--source", "recent", "--pane-id", pane],
    );
    assert!(output.status.success());
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn write_echo_session(directory: &Path, extra_general: &str) -> PathBuf {
    let shell = directory.join("fixture-shell");
    fs::write(
        &shell,
        br#"#!/bin/sh
printf 'READY\n'
while IFS= read -r line; do printf 'OUT:%s\n' "$line"; done
"#,
    )
    .unwrap();
    fs::set_permissions(&shell, fs::Permissions::from_mode(0o700)).unwrap();
    let config = directory.join("config.toml");
    fs::write(
        &config,
        format!(
            "[general]\nshell = {:?}\nrender_interval_ms = 1\n{extra_general}",
            shell.to_str().unwrap()
        ),
    )
    .unwrap();
    config
}

fn create_detached(runtime: &Path, config: &Path, session: &str) {
    let output = common::vvmux_command(runtime)
        .args([
            "--config",
            config.to_str().unwrap(),
            "new",
            "-d",
            "-s",
            session,
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn clients_share_a_session_and_detaching_one_leaves_the_others_intact() {
    let directory = temporary_runtime("vvmc-");
    let runtime = directory.path().to_path_buf();
    let config = write_echo_session(&runtime, "window_size = \"smallest\"\n");
    let session = unique("multi");
    create_detached(&runtime, &config, &session);
    let _guard = SessionGuard {
        runtime: runtime.clone(),
        name: session.clone(),
    };

    let mut first = Client::attach(&runtime, &session, 90, 20, "", &[]);
    first.wait_for_text("READY");
    wait_for_clients(&runtime, &config, &session, "one client", |listing| {
        client_ids(listing).len() == 1
    });
    // Attaching without -d joins: nothing is replaced.
    let mut second = Client::attach(&runtime, &session, 70, 16, "", &[]);
    second.wait_for_text("READY");
    let listing = wait_for_clients(&runtime, &config, &session, "two clients", |listing| {
        client_ids(listing).len() == 2
    });
    // Neither terminal can show media, so neither presents it.
    assert!(listing["presenter_client_id"].is_null(), "{listing}");
    // `smallest` lays the panes out for the smaller terminal, so neither client is clipped.
    assert_eq!(listing["layout"]["columns"], 70, "{listing}");
    assert_eq!(listing["layout"]["rows"], 16, "{listing}");

    // Input from either client reaches the shared pane, and both see the result.
    first.send(b"from-first\n");
    second.wait_for_text("OUT:from-first");
    first.wait_for_text("OUT:from-first");
    second.send(b"from-second\n");
    first.wait_for_text("OUT:from-second");
    second.wait_for_text("OUT:from-second");

    // A prompt belongs to the client that opened it: only that client sees it, and the other
    // client's keys go past it to the focused pane.
    first.send(b"\x02,");
    first.wait_for_text("rename tab");
    second.send(b"past-prompt\n");
    second.wait_for_text("OUT:past-prompt");
    first.wait_for_text("OUT:past-prompt");
    assert!(
        !second.screen_text().contains("rename tab"),
        "another client's prompt was drawn on this one"
    );
    first.send(b"\x1b");
    first.wait_for("the prompt to close", |screen| {
        !screen.visible_text(0).contains("rename tab")
    });

    // A read-only client watches without typing.
    let mut watcher = Client::attach(&runtime, &session, 90, 20, "-r", &[]);
    watcher.wait_for_text("OUT:from-second");
    let listing = wait_for_clients(&runtime, &config, &session, "a watcher", |listing| {
        client_ids(listing).len() == 3
    });
    let read_only = listing["clients"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|client| client["read_only"] == true)
        .count();
    assert_eq!(read_only, 1, "{listing}");
    watcher.send(b"from-watcher\n");
    first.send(b"after-watcher\n");
    watcher.wait_for_text("OUT:after-watcher");
    assert!(
        !pane_text(&runtime, &config, &session, "1").contains("from-watcher"),
        "a read-only client's keys reached the pane"
    );

    // Detaching the second client leaves the others attached and working; its geometry no
    // longer constrains the layout.
    second.send(b"\x02d");
    second.wait_for("the second client to leave", |screen| {
        !screen.alternate_screen()
    });
    let listing = wait_for_clients(&runtime, &config, &session, "two clients left", |listing| {
        client_ids(listing).len() == 2
    });
    assert_eq!(listing["layout"]["columns"], 90, "{listing}");
    first.send(b"after-detach\n");
    first.wait_for_text("OUT:after-detach");
    watcher.wait_for_text("OUT:after-detach");

    // Automation can detach one client by ID without touching another.
    let watcher_id = listing["clients"]
        .as_array()
        .unwrap()
        .iter()
        .find(|client| client["read_only"] == true)
        .and_then(|client| client["client_id"].as_u64())
        .unwrap();
    json(message(
        &runtime,
        &config,
        &session,
        &["detach-client", "--client-id", &watcher_id.to_string()],
    ));
    watcher.wait_for("the watcher to be detached", |screen| {
        !screen.alternate_screen()
    });
    wait_for_clients(&runtime, &config, &session, "one client left", |listing| {
        client_ids(listing).len() == 1
    });
    first.send(b"still-here\n");
    first.wait_for_text("OUT:still-here");
    let missing = message(
        &runtime,
        &config,
        &session,
        &["detach-client", "--client-id", &watcher_id.to_string()],
    );
    assert!(
        !missing.status.success(),
        "a departed client was detached twice"
    );
}

/// An in-process outer Vivid presenter standing in for one client's Vivido window.
struct OuterWindow {
    host: VirtualVivid,
    environment: Vec<(String, String)>,
}

impl OuterWindow {
    const PANE: u64 = 1;

    fn start(columns: u16, rows: u16) -> Self {
        let listener = SocketListener::bind("tcp:127.0.0.1:0").unwrap();
        let endpoint = listener.endpoint();
        let host = VirtualVivid::start_configured_eventless(
            listener,
            PresenterConfig::terminal(MediaConfig::default()),
        )
        .unwrap();
        host.update_metrics(Self::PANE, columns, rows, (8, 16));
        let secret = host.issue_pane_capability(Self::PANE).unwrap();
        let environment = vec![
            ("VIVID_ENDPOINT_CONTROL".to_owned(), endpoint.clone()),
            ("VIVID_ENDPOINT_REALTIME".to_owned(), endpoint.clone()),
            ("VIVID_ENDPOINT_BULK".to_owned(), endpoint),
            ("VIVID_ROOT_SECRET".to_owned(), secret),
        ];
        Self { host, environment }
    }

    fn tracks(&self) -> usize {
        self.host.pane_media_summary(Self::PANE).tracks.len()
    }
}

#[test]
fn media_reaches_only_the_presenter_until_another_client_claims_it() {
    let directory = temporary_runtime("vvmm-");
    let runtime = directory.path().to_path_buf();
    let config = runtime.join("config.toml");
    fs::write(
        &config,
        "[general]\nshell = \"/bin/sh\"\nrender_interval_ms = 1\n",
    )
    .unwrap();
    let session = unique("media");
    create_detached(&runtime, &config, &session);
    let _guard = SessionGuard {
        runtime: runtime.clone(),
        name: session.clone(),
    };

    let local = OuterWindow::start(100, 30);
    let remote = OuterWindow::start(100, 30);
    let _presenter = Client::attach(&runtime, &session, 100, 30, "", &local.environment);
    let listing = wait_for_clients(&runtime, &config, &session, "the presenter", |listing| {
        !listing["presenter_client_id"].is_null()
    });
    let presenter_id = listing["presenter_client_id"].as_u64().unwrap();
    // A second Vivid-capable client joins as a text viewer: the role is taken.
    let mut viewer = Client::attach(&runtime, &session, 100, 30, "", &remote.environment);
    viewer.wait_for("the viewer's alternate screen", |screen| {
        screen.alternate_screen()
    });
    let listing = wait_for_clients(&runtime, &config, &session, "the viewer", |listing| {
        client_ids(listing).len() == 2
    });
    assert_eq!(listing["presenter_client_id"], presenter_id, "{listing}");
    let viewer_id = *client_ids(&listing)
        .iter()
        .find(|id| **id != presenter_id)
        .unwrap();

    let executable = std::env::current_exe().unwrap();
    let program = format!("'{}'", executable.to_str().unwrap().replace('\'', r"'\''"));
    let command =
        format!("{program} --exact multi_client::image_producer_child --ignored --nocapture");
    let opened = json(message(
        &runtime,
        &config,
        &session,
        &["run", &command, "--hold", "--pane-id", "1"],
    ));
    let producer_pane = opened["pane_id"].as_u64().unwrap().to_string();
    let deadline = Instant::now() + Duration::from_secs(30);
    while !pane_text(&runtime, &config, &session, &producer_pane).contains(SHOWN) {
        assert!(
            Instant::now() < deadline,
            "the producer never showed its image"
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    // The image reaches the presenter's window, and never the viewer's.
    let deadline = Instant::now() + Duration::from_secs(20);
    while local.tracks() == 0 {
        assert!(
            Instant::now() < deadline,
            "the presenter never received the image"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(remote.tracks(), 0, "a text viewer received media");

    // The viewer claims the role: the image moves to its window and leaves the old presenter's,
    // which stays attached as a viewer.
    viewer.send(b"\x02M");
    let deadline = Instant::now() + Duration::from_secs(20);
    while remote.tracks() == 0 || local.tracks() != 0 {
        assert!(
            Instant::now() < deadline,
            "the claim did not move the media: local={} remote={}",
            local.tracks(),
            remote.tracks()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let listing = list_clients(&runtime, &config, &session);
    assert_eq!(listing["presenter_client_id"], viewer_id, "{listing}");
    assert_eq!(
        client_ids(&listing).into_iter().collect::<HashSet<_>>(),
        HashSet::from([presenter_id, viewer_id]),
        "the demoted presenter must stay attached"
    );
}
