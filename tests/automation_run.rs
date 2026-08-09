#![cfg(unix)]

//! `vvmux msg run`, end to end against a real detached session.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use serde_json::Value;

struct SessionGuard {
    binary: &'static str,
    name: String,
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        let _ = Command::new(self.binary)
            .args(["kill-session", "--target", &self.name])
            .output();
    }
}

struct Fixture {
    binary: &'static str,
    name: String,
    directory: tempfile::TempDir,
    _guard: SessionGuard,
}

impl Fixture {
    fn start(label: &str) -> Self {
        let binary = env!("CARGO_BIN_EXE_vvmux");
        let directory = tempfile::tempdir().unwrap();
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
        let created = Command::new(binary)
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
            binary,
            name: name.clone(),
            directory,
            _guard: SessionGuard { binary, name },
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
        Command::new(self.binary)
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
