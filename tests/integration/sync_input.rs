#![cfg(unix)]

//! Tab-local sync-input state and owner isolation against real detached sessions.

use crate::common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Output;

use serde_json::Value;

struct Fixture {
    name: String,
    directory: tempfile::TempDir,
}

impl Fixture {
    fn start(label: &str) -> Self {
        // Short `/tmp` root: the runtime directory holds the session socket, whose path must stay
        // inside the platform's `sun_path` limit. Isolating `XDG_CONFIG_HOME` and
        // `XDG_RUNTIME_DIR` keeps a developer's own `startup.toml` and live sessions out of this
        // test's session.
        let directory = tempfile::Builder::new()
            .prefix("vvs-")
            .tempdir_in("/tmp")
            .unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let shell = directory.path().join("sync-shell");
        fs::write(
            &shell,
            br#"#!/bin/sh
printf 'READY pane=%s\r\n' "$VVMUX_PANE_ID"
while IFS= read -r line; do
    printf 'INPUT pane=%s:%s\r\n' "$VVMUX_PANE_ID" "$line"
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
                serde_json::to_string(shell.to_str().unwrap()).unwrap()
            ),
        )
        .unwrap();
        let name = format!("sync-{label}-{}", std::process::id());
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
        let fixture = Self { name, directory };
        fixture.wait_ready(1);
        fixture
    }

    fn msg(&self, arguments: &[&str]) -> Output {
        common::vvmux_command(self.directory.path())
            .args(["msg", "--target", &self.name])
            .args(arguments)
            .output()
            .unwrap()
    }

    fn wait_ready(&self, pane: u64) {
        assert_success(&self.msg(&[
            "wait",
            "text",
            &format!("READY pane={pane}"),
            "--pane-id",
            &pane.to_string(),
            "--timeout",
            "5s",
        ]));
    }

    fn split(&self) -> u64 {
        let opened = json(self.msg(&["split", "vertical", "--pane-id", "1"]));
        let pane = opened["new_pane_id"].as_u64().unwrap();
        self.wait_ready(pane);
        pane
    }

    fn inspect(&self, pane: u64) -> Value {
        json(self.msg(&["inspect", "--pane-id", &pane.to_string()]))
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = common::vvmux_command(self.directory.path())
            .args(["kill-session", "--target", &self.name])
            .output();
    }
}

#[test]
fn sync_state_is_tab_local_and_a_closing_owner_cannot_touch_another() {
    let first = Fixture::start("owner-a");
    let second = Fixture::start("owner-b");
    let first_sibling = first.split();
    let second_sibling = second.split();
    assert_eq!((first_sibling, second_sibling), (2, 2));
    assert_eq!(first.inspect(1)["pane"]["tab_id"], 1);
    assert_eq!(second.inspect(1)["pane"]["tab_id"], 1);

    let enabled = json(first.msg(&["sync-input", "--on", "--pane-id", "1"]));
    assert_eq!(enabled["tab_id"], 1);
    assert_eq!(enabled["sync_input"], true);
    assert_eq!(first.inspect(first_sibling)["pane"]["sync_input"], true);
    assert_eq!(second.inspect(second_sibling)["pane"]["sync_input"], false);

    // Close a same-numbered pane in one owner while its tab is synchronized. The other owner's
    // pane, state, and next valid tab mutation must remain available and independent.
    assert_success(&first.msg(&["close-pane", "--pane-id", &first_sibling.to_string()]));
    assert_eq!(second.inspect(second_sibling)["pane"]["sync_input"], false);
    let enabled = json(second.msg(&[
        "sync-input",
        "--on",
        "--pane-id",
        &second_sibling.to_string(),
    ]));
    assert_eq!(enabled["tab_id"], 1);
    assert_eq!(second.inspect(1)["pane"]["sync_input"], true);
    assert_eq!(first.inspect(1)["pane"]["sync_input"], true);
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "command failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
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
