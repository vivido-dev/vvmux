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
    let flood_marker = directory.path().join("start-flood");
    let shell = directory.path().join("fixture-shell");
    fs::write(
        &shell,
        format!(
            "#!/bin/sh\nprintf 'READY\\n'\nwhile ! test -f {}; do sleep 0.01; done\ni=0\nwhile test \"$i\" -lt 1000; do printf '\\r%04d' \"$i\"; i=$((i + 1)); sleep 0.003; done\nprintf '\\nFLOOD-DONE\\n'\nwhile :; do sleep 60; done\n",
            serde_json::to_string(flood_marker.to_str().unwrap()).unwrap()
        ),
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
    fs::create_dir(package.join("schemas")).unwrap();
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
[[events]]
on = "pane.screen_changed"
handler = "screen"
[[actions]]
id = "identity"
title = "Identity"
description = "Return the runtime identity"
handler = "identity"
input_schema = "schemas/input.json"
output_schema = "schemas/output.json"
timeout_ms = 5000
[[actions]]
id = "crash"
title = "Crash"
description = "Crash the runtime"
handler = "crash"
input_schema = "schemas/input.json"
output_schema = "schemas/output.json"
timeout_ms = 5000
"#,
    )
    .unwrap();
    fs::write(
        package.join("schemas/input.json"),
        r#"{"type":"object","additionalProperties":false}"#,
    )
    .unwrap();
    fs::write(
        package.join("schemas/output.json"),
        r#"{"type":"object","required":["session","instance"],"properties":{"session":{"type":"string"},"instance":{"type":"string"}},"additionalProperties":false}"#,
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
    let peer_name = format!("plugin-events-peer-{}", std::process::id());
    assert_success(
        &command(binary, &runtime, &config_home)
            .args([
                "--config",
                config.to_str().unwrap(),
                "new",
                "-s",
                &peer_name,
                "-d",
            ])
            .output()
            .unwrap(),
    );
    let _peer_guard = SessionGuard {
        binary,
        name: peer_name.clone(),
        runtime: runtime.clone(),
        config_home: config_home.clone(),
    };
    wait_for_file(&package.join(format!("activated-{name}")));
    wait_for_file(&package.join(format!("events-{name}.ndjson")));
    wait_for_file(&package.join(format!("activated-{peer_name}")));
    wait_for_file(&package.join(format!("events-{peer_name}.ndjson")));
    let hook = read_plugin_events(&package, &name)
        .into_iter()
        .find(|event| event["name"] == "opened")
        .expect("pane.opened hook was not delivered");
    assert_eq!(hook["type"], "event");
    assert_eq!(hook["name"], "opened");
    assert_eq!(hook["payload"]["event"], "pane.opened");
    assert_eq!(hook["context"]["causation_depth"], 1);
    assert_eq!(hook["context"]["pane_id"], 1);

    let events = replay_events(binary, &runtime, &config_home, &name);
    assert!(
        events
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .any(|event| event["type"] == "event" && event["name"] == "pane.opened"),
        "replay did not contain pane.opened: {events}"
    );

    let first = invoke_identity(binary, &runtime, &config_home, &name);
    let peer = invoke_identity(binary, &runtime, &config_home, &peer_name);
    assert_eq!(first["session"], name);
    assert_eq!(peer["session"], peer_name);
    assert_ne!(first["instance"], peer["instance"]);
    let peer_instance = peer["instance"].clone();

    let crashed = command(binary, &runtime, &config_home)
        .args(["plugin", "invoke", "dev.events/crash", "--target", &name])
        .output()
        .unwrap();
    assert!(!crashed.status.success());
    assert!(String::from_utf8_lossy(&crashed.stderr).contains("runtime_crashed"));
    let crashed_events = replay_events(binary, &runtime, &config_home, &name);
    assert!(
        crashed_events
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .any(|event| event["name"] == "plugin.runtime_crashed"
                && event["payload"]["plugin_id"] == "dev.events")
    );
    let peer_events = replay_events(binary, &runtime, &config_home, &peer_name);
    assert!(!peer_events.contains("plugin.runtime_crashed"));
    assert_eq!(
        invoke_identity(binary, &runtime, &config_home, &peer_name)["instance"],
        peer_instance,
        "one session's crash must not restart the peer's same-ID runtime"
    );

    let deadline = Instant::now() + Duration::from_secs(3);
    let restarted = loop {
        let output = command(binary, &runtime, &config_home)
            .args(["plugin", "invoke", "dev.events/identity", "--target", &name])
            .output()
            .unwrap();
        if output.status.success() {
            break serde_json::from_slice::<Value>(&output.stdout).unwrap();
        }
        assert!(Instant::now() < deadline, "runtime did not restart");
        thread::sleep(Duration::from_millis(25));
    };
    assert_ne!(restarted["instance"], first["instance"]);
    assert_eq!(restarted["session"], name);

    fs::write(package.join("slow-event-ms"), b"25").unwrap();
    fs::write(&flood_marker, b"start").unwrap();
    for session in [&name, &peer_name] {
        let started = Instant::now();
        assert_success(
            &command(binary, &runtime, &config_home)
                .args(["msg", "--target", session, "list-panes"])
                .output()
                .unwrap(),
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "plugin event firehose delayed unrelated automation for {session}"
        );
        let started = Instant::now();
        assert_success(
            &command(binary, &runtime, &config_home)
                .args(["msg", "--target", session, "reload-config"])
                .output()
                .unwrap(),
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "plugin event firehose delayed config reload for {session}"
        );
    }
    wait_for_event(&package, &name, "vvmux.event_gap");
    wait_for_event(&package, &peer_name, "vvmux.event_gap");
}

fn invoke_identity(binary: &str, runtime: &Path, config_home: &Path, session: &str) -> Value {
    let output = command(binary, runtime, config_home)
        .args([
            "plugin",
            "invoke",
            "dev.events/identity",
            "--target",
            session,
        ])
        .output()
        .unwrap();
    assert_success(&output);
    serde_json::from_slice(&output.stdout).unwrap()
}

fn replay_events(binary: &str, runtime: &Path, config_home: &Path, name: &str) -> String {
    let mut stream = command(binary, runtime, config_home)
        .args(["plugin", "events", "--target", name, "--after", "0"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    thread::sleep(Duration::from_millis(300));
    stream.kill().unwrap();
    String::from_utf8(stream.wait_with_output().unwrap().stdout).unwrap()
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

fn wait_for_event(package: &Path, session: &str, expected: &str) {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        if read_plugin_events(package, session)
            .iter()
            .any(|event| event["payload"]["event"] == expected)
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {expected} in {session}"
        );
        thread::sleep(Duration::from_millis(25));
    }
}

fn read_plugin_events(package: &Path, session: &str) -> Vec<Value> {
    fs::read_to_string(package.join(format!("events-{session}.ndjson")))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
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
