#![cfg(unix)]

//! Live config reload, end to end against a real detached session.
//!
//! The observable proof that a reload took effect is pane geometry: toggling `status_visible`
//! moves the status row in or out of the pane area, so `msg inspect` reports a different pane
//! height. That routes through display re-normalization and a relayout, which is exactly the part
//! of the reload path most likely to break.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
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
    config: std::path::PathBuf,
    _directory: tempfile::TempDir,
    _guard: SessionGuard,
}

impl Fixture {
    fn start(label: &str) -> Self {
        let binary = env!("CARGO_BIN_EXE_vvmux");
        let directory = tempfile::tempdir().unwrap();
        let shell = directory.path().join("fixture-shell");
        fs::write(
            &shell,
            br#"#!/bin/sh
printf 'READY pane=%s\n' "$VVMUX_PANE_ID"
while IFS= read -r line; do :; done
"#,
        )
        .unwrap();
        fs::set_permissions(&shell, fs::Permissions::from_mode(0o700)).unwrap();

        let config = directory.path().join("vvmux.toml");
        let name = format!("reload-{label}-{}", std::process::id());
        write_config(&config, &shell, "status_visible = true");

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
        let guard = SessionGuard {
            binary,
            name: name.clone(),
        };

        let fixture = Fixture {
            binary,
            name,
            config,
            _directory: directory,
            _guard: guard,
        };
        fixture.wait_ready();
        fixture
    }

    fn wait_ready(&self) {
        let output = self.msg(&[
            "wait",
            "text",
            "READY pane=1",
            "--pane-id",
            "1",
            "--timeout",
            "5s",
        ]);
        assert_success(&output);
    }

    fn msg(&self, arguments: &[&str]) -> Output {
        Command::new(self.binary)
            .args(["msg", "--target", &self.name])
            .args(arguments)
            .output()
            .unwrap()
    }

    /// The focused pane's height, which shrinks by one row while the status bar is visible.
    fn pane_height(&self) -> u64 {
        let inspected = json(self.msg(&["inspect", "--pane-id", "1"]));
        inspected["pane"]["geometry"]["height"]
            .as_u64()
            .unwrap_or_else(|| panic!("no pane geometry in {inspected}"))
    }

    fn rewrite(&self, general: &str) {
        let shell = self._directory.path().join("fixture-shell");
        write_config(&self.config, &shell, general);
    }
}

fn write_config(path: &Path, shell: &Path, general_extra: &str) {
    fs::write(
        path,
        format!(
            "[general]\nshell = {}\nrender_interval_ms = 1\n{general_extra}\n",
            serde_json::to_string(shell.to_str().unwrap()).unwrap()
        ),
    )
    .unwrap();
}

#[test]
fn an_explicit_reload_applies_a_changed_status_bar() {
    let fixture = Fixture::start("explicit");

    let capabilities = json(fixture.msg(&["capabilities"]));
    assert!(
        capabilities["methods"]
            .as_array()
            .unwrap()
            .iter()
            .any(|method| method == "reload_config"),
        "reload_config must be advertised"
    );

    let with_status = fixture.pane_height();

    fixture.rewrite("status_visible = false");
    let reloaded = json(fixture.msg(&["reload-config"]));
    assert_eq!(reloaded["reloaded"], true);
    assert!(reloaded["path"].as_str().unwrap().ends_with("vvmux.toml"));

    assert_eq!(
        fixture.pane_height(),
        with_status + 1,
        "hiding the status bar must give its row back to the pane"
    );
}

#[test]
fn an_invalid_config_is_rejected_and_the_session_keeps_running() {
    let fixture = Fixture::start("invalid");
    let before = fixture.pane_height();

    // In range 1..=1000, so this fails validation rather than parsing.
    fixture.rewrite("render_interval_ms = 99999");
    let rejected = fixture.msg(&["reload-config"]);
    assert!(!rejected.status.success(), "an invalid config must fail");
    let described = String::from_utf8_lossy(&rejected.stderr).to_string();
    assert!(described.contains("invalid_config"), "{described}");

    assert_eq!(
        fixture.pane_height(),
        before,
        "a rejected reload must not disturb the running layout"
    );
    // Still serving: the session survived a bad config.
    assert_success(&fixture.msg(&["list-panes"]));
}

#[test]
fn a_media_change_is_reported_as_ignored_rather_than_applied() {
    let fixture = Fixture::start("media");

    fixture.rewrite("status_visible = true\n\n[media]\nmax_sources = 7");
    let reloaded = json(fixture.msg(&["reload-config"]));

    assert_eq!(reloaded["reloaded"], true);
    assert!(
        reloaded["ignored"]
            .as_array()
            .unwrap()
            .iter()
            .any(|section| section == "media"),
        "media is owned by the running presenter and must be reported as ignored: {reloaded}"
    );
    assert_success(&fixture.msg(&["list-panes"]));
}

#[test]
fn a_server_change_is_reported_as_ignored_rather_than_adopted() {
    let fixture = Fixture::start("server");

    fixture.rewrite("status_visible = true\n\n[server]\nmax_connections = 7");
    let reloaded = json(fixture.msg(&["reload-config"]));

    assert!(
        reloaded["ignored"]
            .as_array()
            .unwrap()
            .iter()
            .any(|section| section == "server"),
        "the separate gateway process owns server config: {reloaded}"
    );
}

#[test]
fn a_deferred_section_is_named_in_the_report() {
    let fixture = Fixture::start("deferred");

    fixture.rewrite(
        "status_visible = true\nprefix = 'C-a'\nscrollback_lines = 4242\n\n[keys.prefix]\ng = 'new-tab'",
    );
    let reloaded = json(fixture.msg(&["reload-config"]));

    assert!(
        reloaded["deferred"]
            .as_array()
            .unwrap()
            .iter()
            .any(|section| section == "general.pane_defaults"),
        "a pane-spawn setting only affects future panes and must say so: {reloaded}"
    );
    for section in ["general.prefix", "keys.prefix"] {
        assert!(
            reloaded["deferred"]
                .as_array()
                .unwrap()
                .iter()
                .any(|value| value == section),
            "client-owned {section} must be reported separately: {reloaded}"
        );
    }
}

/// The watcher, with no explicit reload: the change must be picked up on its own.
#[test]
fn the_watcher_notices_an_edit_without_being_asked() {
    let fixture = Fixture::start("watch");
    let with_status = fixture.pane_height();

    fixture.rewrite("status_visible = false");

    // One poll interval plus one debounce interval, with generous slack for a loaded machine.
    let deadline = Instant::now() + Duration::from_secs(20);
    let target = with_status + 1;
    while Instant::now() < deadline {
        if fixture.pane_height() == target {
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    panic!("the config watcher never applied the edit");
}

/// SIGUSR1 reloads, and — the regression this guards — does not fall into the shutdown path that
/// every other trapped signal takes.
#[test]
fn sigusr1_reloads_without_terminating_the_session() {
    let fixture = Fixture::start("sigusr1");
    let with_status = fixture.pane_height();

    let listed = String::from_utf8(
        Command::new("pgrep")
            .args(["-f", &format!("__server --session {}", fixture.name)])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    let pid: i32 = listed
        .lines()
        .next()
        .unwrap_or_else(|| panic!("no server process for {}", fixture.name))
        .trim()
        .parse()
        .unwrap();

    fixture.rewrite("status_visible = false");
    assert!(
        Command::new("kill")
            .args(["-USR1", &pid.to_string()])
            .status()
            .unwrap()
            .success()
    );

    let deadline = Instant::now() + Duration::from_secs(10);
    let target = with_status + 1;
    while Instant::now() < deadline {
        if fixture.pane_height() == target {
            // The session must still be serving; a signal that reloads must not also shut down.
            assert_success(&fixture.msg(&["list-panes"]));
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("SIGUSR1 did not apply the config change");
}

/// Deleting the config must not silently reset a live session to defaults.
#[test]
fn a_deleted_config_leaves_the_running_session_alone() {
    let fixture = Fixture::start("deleted");
    let before = fixture.pane_height();

    fs::remove_file(&fixture.config).unwrap();
    std::thread::sleep(Duration::from_secs(3));

    assert_eq!(
        fixture.pane_height(),
        before,
        "a missing config must leave the last good one in force"
    );
    let rejected = fixture.msg(&["reload-config"]);
    assert!(
        !rejected.status.success(),
        "an explicit reload of a deleted file must say so"
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

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "command failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
