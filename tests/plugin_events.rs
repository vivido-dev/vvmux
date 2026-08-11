#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

struct SessionGuard {
    binary: &'static str,
    name: String,
    runtime: PathBuf,
    config_home: PathBuf,
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        let _ = command(self.binary, &self.runtime, &self.config_home)
            .args(["kill-session", "--target", &self.name])
            .output();
    }
}

#[test]
fn session_activation_hooks_and_bounded_replay_are_live() {
    let binary = env!("CARGO_BIN_EXE_vvmux");
    let directory = tempfile::tempdir().unwrap();
    let runtime = directory.path().join("runtime");
    let config_home = directory.path().join("config");
    private_directory(&runtime);
    private_directory(&config_home);
    let shell = directory.path().join("fixture-shell");
    fs::write(
        &shell,
        b"#!/bin/sh\nprintf 'READY\\n'\nwhile :; do sleep 60; done\n",
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
    let package = directory.path().join("plugin");
    fs::create_dir(&package).unwrap();
    fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/native_event_plugin.py"),
        package.join("plugin.py"),
    )
    .unwrap();
    fs::write(
        package.join("vvmux-plugin.toml"),
        r#"manifest_version = 1
[plugin]
id = "dev.events"
name = "Events"
version = "1.0.0"
min_vvmux_version = "0.4.0"
description = "Event fixture"
platforms = ["linux", "macos"]
permissions = ["events.subscribe"]
[runtime]
kind = "process"
command = ["python3", "plugin.py"]
activation = "session"
[[events]]
on = "pane.opened"
handler = "opened"
"#,
    )
    .unwrap();
    assert_success(
        &command(binary, &runtime, &config_home)
            .args(["plugin", "link", package.to_str().unwrap(), "--yes"])
            .output()
            .unwrap(),
    );

    let name = format!("plugin-events-{}", std::process::id());
    assert_success(
        &command(binary, &runtime, &config_home)
            .args([
                "--config",
                config.to_str().unwrap(),
                "new",
                "-s",
                &name,
                "-d",
            ])
            .output()
            .unwrap(),
    );
    let _guard = SessionGuard {
        binary,
        name: name.clone(),
        runtime: runtime.clone(),
        config_home: config_home.clone(),
    };
    wait_for_file(&package.join("activated"));
    wait_for_file(&package.join("events.ndjson"));
    let hook: Value = serde_json::from_str(
        fs::read_to_string(package.join("events.ndjson"))
            .unwrap()
            .lines()
            .next()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(hook["type"], "event");
    assert_eq!(hook["name"], "opened");
    assert_eq!(hook["payload"]["event"], "pane.opened");
    assert_eq!(hook["context"]["causation_depth"], 1);

    let mut stream = command(binary, &runtime, &config_home)
        .args(["plugin", "events", "--target", &name, "--after", "0"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    thread::sleep(Duration::from_millis(300));
    stream.kill().unwrap();
    let output = stream.wait_with_output().unwrap();
    let events = String::from_utf8(output.stdout).unwrap();
    assert!(
        events
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .any(|event| event["type"] == "event" && event["name"] == "pane.opened"),
        "replay did not contain pane.opened: {events}"
    );
}

fn wait_for_file(path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {}",
            path.display()
        );
        thread::sleep(Duration::from_millis(20));
    }
}

fn command(binary: &str, runtime: &Path, config_home: &Path) -> Command {
    let mut command = Command::new(binary);
    command
        .env("XDG_RUNTIME_DIR", runtime)
        .env("XDG_CONFIG_HOME", config_home)
        .env_remove("VIVID_ENDPOINT")
        .env_remove("VIVID_TOKEN");
    command
}

fn private_directory(path: &Path) {
    fs::create_dir_all(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "command failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
