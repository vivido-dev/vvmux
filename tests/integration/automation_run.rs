#![cfg(unix)]

//! `vvmux msg run`, end to end against a real detached session.

use crate::common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Output;
use std::time::{Duration, Instant};

use serde_json::Value;

struct SessionGuard {
    runtime: PathBuf,
    name: String,
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        let _ = common::vvmux_command(&self.runtime)
            .args(["kill-session", "--target", &self.name])
            .output();
    }
}

struct Fixture {
    name: String,
    _guard: SessionGuard,
    // Fields drop in declaration order. Keep the runtime directory alive until the guard has
    // connected to and killed the detached session, or the server and pane become orphans.
    directory: tempfile::TempDir,
}

impl Fixture {
    fn start(label: &str) -> Self {
        // Short `/tmp` root: the runtime directory holds the session socket, whose path must stay
        // inside the platform's `sun_path` limit. Isolating `XDG_CONFIG_HOME` and
        // `XDG_RUNTIME_DIR` keeps a developer's own `startup.toml` and live sessions out of this
        // test's session.
        let directory = tempfile::Builder::new()
            .prefix("vvr-")
            .tempdir_in("/tmp")
            .unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        // A shell that idles forever, so the anchor pane never disappears mid-test.
        let shell = directory.path().join("fixture-shell");
        fs::write(
            &shell,
            br#"#!/bin/sh
if [ "$1" = "-c" ]; then
    shift
    exec /bin/sh -c "$@"
fi
printf 'READY pane=%s\n' "$VVMUX_PANE_ID"
while IFS= read -r line; do :; done
"#,
        )
        .unwrap();
        fs::set_permissions(&shell, fs::Permissions::from_mode(0o700)).unwrap();

        let config = directory.path().join("vvmux.toml");
        fs::write(
            &config,
            format!(
                "[general]\nshell = {}\nrender_interval_ms = 1\n",
                serde_json::to_string(shell.to_str().unwrap()).unwrap()
            ),
        )
        .unwrap();

        let name = format!("run-{label}-{}", std::process::id());
        let created = common::vvmux_command(directory.path())
            .args([
                "--config",
                config.to_str().unwrap(),
                "new",
                "-s",
                &name,
                "-d",
            ])
            .output()
            .unwrap();
        assert_success(&created);

        let fixture = Fixture {
            name: name.clone(),
            _guard: SessionGuard {
                runtime: directory.path().to_path_buf(),
                name,
            },
            directory,
        };
        assert_success(&fixture.msg(&[
            "wait",
            "text",
            "READY pane=1",
            "--pane-id",
            "1",
            "--timeout",
            "5s",
        ]));
        fixture
    }

    fn msg(&self, arguments: &[&str]) -> Output {
        common::vvmux_command(self.directory.path())
            .args(["msg", "--target", &self.name])
            .args(arguments)
            .output()
            .unwrap()
    }

    fn pane_ids(&self) -> Vec<u64> {
        json(self.msg(&["list-panes"]))["panes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|pane| pane["pane_id"].as_u64().unwrap())
            .collect()
    }

    fn wait_until_gone(&self, pane: u64) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if !self.pane_ids().contains(&pane) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("pane {pane} never closed");
    }
}

#[test]
fn a_run_pane_executes_its_command_and_closes_when_it_finishes() {
    let fixture = Fixture::start("basic");

    let opened = json(fixture.msg(&["run", "printf 'RUN_OK\\n'; sleep 30", "--pane-id", "1"]));
    let pane = opened["pane_id"].as_u64().unwrap();
    assert_eq!(pane, 2);
    assert_eq!(opened["tab_id"], 1);

    assert_success(&fixture.msg(&[
        "wait",
        "text",
        "RUN_OK",
        "--pane-id",
        &pane.to_string(),
        "--timeout",
        "5s",
    ]));
    assert_eq!(fixture.pane_ids(), vec![1, 2]);

    // Without --hold, a finished command takes its pane with it.
    let short = json(fixture.msg(&["run", "true", "--pane-id", "1"]));
    fixture.wait_until_gone(short["pane_id"].as_u64().unwrap());
}

#[test]
fn a_held_pane_survives_its_command_and_still_resolves_wait_exit() {
    let fixture = Fixture::start("hold");

    let opened = json(fixture.msg(&["run", "exit 3", "--hold", "--pane-id", "1"]));
    let pane = opened["pane_id"].as_u64().unwrap();

    let exited = json(fixture.msg(&[
        "wait",
        "exit",
        "--pane-id",
        &pane.to_string(),
        "--timeout",
        "5s",
    ]));
    assert_eq!(exited["code"], 3, "the real exit code must reach a waiter");
    assert_eq!(exited["success"], false);

    // The whole point of --hold: the pane is still there afterwards.
    assert!(
        fixture.pane_ids().contains(&pane),
        "a held pane must outlive its command"
    );
    let shown = String::from_utf8(
        fixture
            .msg(&["get-text", "--pane-id", &pane.to_string()])
            .stdout,
    )
    .unwrap();
    assert!(
        shown.contains("exited 3"),
        "the pane should say how it ended, got {shown:?}"
    );
}

#[test]
fn a_run_pane_can_float_or_open_its_own_tab() {
    let fixture = Fixture::start("placement");

    let floated = json(fixture.msg(&["run", "sleep 30", "--placement", "float", "--pane-id", "1"]));
    let float_pane = floated["pane_id"].as_u64().unwrap();
    assert_eq!(floated["tab_id"], 1, "a float stays in the anchor's tab");
    let inspected = json(fixture.msg(&["inspect", "--pane-id", &float_pane.to_string()]));
    assert_eq!(inspected["pane"]["layer"], "floating");
    let rejected = fixture.msg(&[
        "run",
        "true",
        "--placement",
        "split",
        "--pane-id",
        &float_pane.to_string(),
    ]);
    assert!(!rejected.status.success());
    let diagnostic = String::from_utf8_lossy(&rejected.stderr);
    assert!(diagnostic.contains("invalid_state"), "{diagnostic}");
    assert!(
        diagnostic.contains("cannot split a floating pane"),
        "{diagnostic}"
    );

    let tabbed = json(fixture.msg(&["run", "sleep 30", "--placement", "tab", "--pane-id", "1"]));
    assert_eq!(tabbed["tab_id"], 2, "a tab placement opens a new tab");
    let inspected = json(fixture.msg(&[
        "inspect",
        "--pane-id",
        &tabbed["pane_id"].as_u64().unwrap().to_string(),
    ]));
    assert_eq!(inspected["pane"]["tab_id"], 2);
}

#[test]
fn run_honors_an_explicit_working_directory() {
    let fixture = Fixture::start("cwd");
    let elsewhere = fixture.directory.path().join("elsewhere");
    fs::create_dir(&elsewhere).unwrap();

    let opened = json(fixture.msg(&[
        "run",
        "pwd",
        "--hold",
        "--cwd",
        elsewhere.to_str().unwrap(),
        "--pane-id",
        "1",
    ]));
    let pane = opened["pane_id"].as_u64().unwrap();
    assert_success(&fixture.msg(&[
        "wait",
        "exit",
        "--pane-id",
        &pane.to_string(),
        "--timeout",
        "5s",
    ]));

    let shown = String::from_utf8(
        fixture
            .msg(&["get-text", "--pane-id", &pane.to_string()])
            .stdout,
    )
    .unwrap();
    // macOS reports /private/var for /var, so compare on the final component.
    let leaf = elsewhere.file_name().unwrap().to_str().unwrap();
    assert!(
        shown.contains(leaf),
        "expected the command to run in {leaf}, got {shown:?}"
    );
}

#[test]
fn run_rejects_a_command_it_cannot_honor() {
    let fixture = Fixture::start("invalid");

    let empty = fixture.msg(&["run", "   ", "--pane-id", "1"]);
    assert!(!empty.status.success());
    assert!(
        String::from_utf8_lossy(&empty.stderr).contains("invalid_params"),
        "{}",
        String::from_utf8_lossy(&empty.stderr)
    );

    let missing_cwd = fixture.msg(&[
        "run",
        "true",
        "--cwd",
        "/no/such/directory/anywhere",
        "--pane-id",
        "1",
    ]);
    assert!(!missing_cwd.status.success());

    // A rejected run must not have created anything.
    assert_eq!(fixture.pane_ids(), vec![1]);
}

#[test]
fn reset_preserves_running_pty_and_history_and_respawn_replaces_only_on_success() {
    let fixture = Fixture::start("lifecycle");
    let command = r#"printf 'SAVE-ME\n'; i=0; while [ "$i" -lt 40 ]; do echo padding; i=$((i+1)); done; printf '\033[?1049h\033[?2004hWEDGED'; while IFS= read -r line; do printf 'alive:%s\n' "$line"; done"#;
    let opened = json(fixture.msg(&["run", command, "--hold", "--pane-id", "1"]));
    let pane = opened["pane_id"].as_u64().unwrap().to_string();
    let inspect = |id: &str| json(fixture.msg(&["inspect", "--pane-id", id]))["pane"].clone();
    let wait = |text: &str, id: &str| {
        assert_success(&fixture.msg(&["wait", "text", text, "--pane-id", id, "--timeout", "5s"]));
    };
    wait("WEDGED", &pane);
    assert_success(&fixture.msg(&["pane-rename", "--pane-id", &pane, "--name", "worker"]));
    let before = inspect(&pane);
    assert_eq!(before["screen"], "alternate");
    let reset = json(fixture.msg(&["reset-pane", "--pane-id", &pane]));
    assert_eq!(reset["pane_id"], opened["pane_id"]);
    let after = inspect(&pane);
    assert_eq!(after["screen"], "primary");
    assert_eq!(after["history_size"], before["history_size"]);
    assert_eq!(after["process"]["pid"], before["process"]["pid"]);
    assert_eq!(after["geometry"], before["geometry"]);
    let text = fixture.msg(&["get-text", "--rows", "200", "--pane-id", &pane]);
    assert_success(&text);
    assert!(String::from_utf8_lossy(&text.stdout).contains("SAVE-ME"));
    assert!(!String::from_utf8_lossy(&text.stdout).contains("WEDGED"));

    let shell = fixture.directory.path().join("fixture-shell");
    let unavailable = shell.with_extension("unavailable");
    fs::rename(&shell, &unavailable).unwrap();
    let failed = fixture.msg(&["respawn-pane", "--pane-id", &pane]);
    assert!(!failed.status.success());
    assert!(String::from_utf8_lossy(&failed.stderr).contains("spawn_failed"));
    assert_eq!(inspect(&pane)["process"]["pid"], before["process"]["pid"]);
    assert_success(&fixture.msg(&["submit", "original", "--pane-id", &pane]));
    wait("alive:original", &pane);
    fs::rename(&unavailable, &shell).unwrap();

    let replaced = json(fixture.msg(&["respawn-pane", "--pane-id", &pane]));
    assert_eq!(replaced["previous_pane_id"], opened["pane_id"]);
    assert_ne!(replaced["pane_id"], replaced["previous_pane_id"]);
    let fresh = replaced["pane_id"].as_u64().unwrap().to_string();
    wait("WEDGED", &fresh);
    assert_eq!(inspect(&fresh)["geometry"], before["geometry"]);
    let named = json(fixture.msg(&["--pane-name", "worker", "inspect"]))["pane"].clone();
    assert_eq!(named["pane_id"], replaced["pane_id"]);
    assert!(
        !fixture
            .msg(&["inspect", "--pane-id", &pane])
            .status
            .success()
    );
}

#[test]
fn failed_respawn_keeps_a_held_exited_pane_inspectable() {
    let fixture = Fixture::start("corpse");
    let opened = json(fixture.msg(&["run", "echo final-mark; exit 7", "--hold", "--pane-id", "1"]));
    let pane = opened["pane_id"].as_u64().unwrap().to_string();
    assert_success(&fixture.msg(&["wait", "exit", "--pane-id", &pane, "--timeout", "5s"]));
    let shell = fixture.directory.path().join("fixture-shell");
    fs::rename(&shell, shell.with_extension("unavailable")).unwrap();
    assert!(
        !fixture
            .msg(&["respawn-pane", "--pane-id", &pane])
            .status
            .success()
    );
    assert_eq!(
        json(fixture.msg(&["inspect", "--pane-id", &pane]))["pane"]["process"]["exit"]["code"],
        7
    );
    let text = fixture.msg(&["get-text", "--pane-id", &pane]);
    assert_success(&text);
    assert!(String::from_utf8_lossy(&text.stdout).contains("final-mark"));
}

fn json(output: Output) -> Value {
    assert_success(&output);
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "invalid JSON ({error}): {}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "command failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
