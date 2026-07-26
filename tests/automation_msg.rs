#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};

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

#[test]
fn pane_automation_drives_and_observes_only_the_selected_pane() {
    let binary = env!("CARGO_BIN_EXE_vvmux");
    let directory = tempfile::tempdir().unwrap();
    let shell = directory.path().join("fixture-shell");
    fs::write(
        &shell,
        br#"#!/bin/sh
printf 'READY pane=%s tab=%s\n' "$VVMUX_PANE_ID" "$VVMUX_TAB_ID"
while IFS= read -r line; do
    if [ "$line" = exit ]; then
        exit 0
    fi
    printf 'OUT pane=%s:%s\n' "$VVMUX_PANE_ID" "$line"
done
"#,
    )
    .unwrap();
    fs::set_permissions(&shell, fs::Permissions::from_mode(0o700)).unwrap();
    let config = directory.path().join("vvmux.toml");
    fs::write(
        &config,
        format!(
            "[general]\nshell = {}\nrender_interval_ms = 1\n",
            toml_string(&shell)
        ),
    )
    .unwrap();

    let name = format!("automation-test-{}", std::process::id());
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
    let _guard = SessionGuard {
        binary,
        name: name.clone(),
    };

    wait_text(binary, &name, 1, "READY pane=1 tab=1");
    let right = json(command(
        binary,
        &name,
        &["split", "vertical", "--pane-id", "1"],
    ));
    assert_eq!(right["new_pane_id"], 2);
    let bottom_left = json(command(
        binary,
        &name,
        &["split", "horizontal", "--pane-id", "1"],
    ));
    assert_eq!(bottom_left["new_pane_id"], 3);
    let bottom_right = json(command(
        binary,
        &name,
        &["split", "horizontal", "--pane-id", "2"],
    ));
    assert_eq!(bottom_right["new_pane_id"], 4);

    for pane in 1..=4 {
        wait_text(binary, &name, pane, &format!("READY pane={pane} tab=1"));
    }

    assert_success(&command(
        binary,
        &name,
        &["typing", "hello-top-right", "--pane-id", "2"],
    ));
    assert_success(&command(binary, &name, &["key", "Enter", "--pane-id", "2"]));
    wait_text(binary, &name, 2, "OUT pane=2:hello-top-right");

    let top_right = text(command(binary, &name, &["get-text", "--pane-id", "2"]));
    assert!(top_right.contains("OUT pane=2:hello-top-right"));
    for pane in [1, 3, 4] {
        let output = text(command(
            binary,
            &name,
            &["get-text", "--pane-id", &pane.to_string()],
        ));
        assert!(
            !output.contains("hello-top-right"),
            "output leaked into pane {pane}"
        );
    }
    let listed = json(command(binary, &name, &["list-panes"]));
    assert_eq!(
        listed["panes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|pane| pane["pane_id"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        [1, 2, 3, 4]
    );
    let focused = text(command(binary, &name, &["get-text"]));
    assert!(focused.contains("READY pane=4 tab=1"));
    let inherited = Command::new(binary)
        .args(["msg", "get-text"])
        .env("VVMUX_SESSION", &name)
        .env("VVMUX_PANE_ID", "2")
        .output()
        .unwrap();
    assert!(text(inherited).contains("READY pane=2 tab=1"));

    let grid = json(command(binary, &name, &["get-grid", "--pane-id", "2"]));
    assert_eq!(grid["pane_id"], 2);
    assert!(grid["full"].as_bool().unwrap());
    assert!(grid["grid"]["columns"].as_u64().unwrap() >= 4);
    assert!(grid["grid"]["rows"].as_u64().unwrap() >= 2);
    let grid_text = grid["rows"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|row| row["cells"].as_array().unwrap())
        .filter_map(|cell| cell["text"].as_str())
        .collect::<String>();
    assert!(grid_text.contains("hello-top-right"));

    let sequence = grid["screen_sequence"].as_u64().unwrap();
    let unchanged = json(command(
        binary,
        &name,
        &[
            "get-grid",
            "--pane-id",
            "2",
            "--since-screen",
            &sequence.to_string(),
        ],
    ));
    assert!(!unchanged["full"].as_bool().unwrap());
    assert!(unchanged["rows"].as_array().unwrap().is_empty());

    let stable = json(command(
        binary,
        &name,
        &[
            "wait",
            "screen-stable",
            "--pane-id",
            "2",
            "--quiet",
            "1ms",
            "--timeout",
            "2s",
        ],
    ));
    assert_eq!(stable["pane_id"], 2);

    let trace = json(command(
        binary,
        &name,
        &["trace-media", "--pane-id", "2", "--limit", "16"],
    ));
    assert!(trace["current_sequence"].is_u64());
    assert!(trace["oldest_sequence"].is_u64());
    assert!(trace["events"].as_array().is_some());

    assert_success(&command(
        binary,
        &name,
        &["typing", "exit", "--pane-id", "4"],
    ));
    assert_success(&command(binary, &name, &["key", "Enter", "--pane-id", "4"]));
    let exited = json(command(
        binary,
        &name,
        &["wait", "exit", "--pane-id", "4", "--timeout", "2s"],
    ));
    assert_eq!(exited["code"], 0);
    assert_eq!(exited["success"], true);
    let stale = command(binary, &name, &["get-text", "--pane-id", "4"]);
    assert!(!stale.status.success());
    assert!(String::from_utf8_lossy(&stale.stderr).contains("pane_not_found"));
}

fn command(binary: &str, session: &str, arguments: &[&str]) -> Output {
    Command::new(binary)
        .args(["msg", "--target", session])
        .args(arguments)
        .output()
        .unwrap()
}

fn wait_text(binary: &str, session: &str, pane: u64, pattern: &str) {
    let output = command(
        binary,
        session,
        &[
            "wait",
            "text",
            pattern,
            "--pane-id",
            &pane.to_string(),
            "--timeout",
            "5s",
        ],
    );
    assert_success(&output);
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

fn text(output: Output) -> String {
    assert_success(&output);
    String::from_utf8(output.stdout).unwrap()
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "command failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn toml_string(path: &Path) -> String {
    serde_json::to_string(path.to_str().unwrap()).unwrap()
}
