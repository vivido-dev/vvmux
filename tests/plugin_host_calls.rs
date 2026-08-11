#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
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
fn native_service_host_calls_are_scoped_capability_checked_and_actor_safe() {
    let binary = env!("CARGO_BIN_EXE_vvmux");
    let directory = tempfile::tempdir().unwrap();
    let runtime = directory.path().join("runtime");
    let config_home = directory.path().join("config");
    private_directory(&runtime);
    private_directory(&config_home);

    let shell = directory.path().join("fixture-shell");
    fs::write(
        &shell,
        br#"#!/bin/sh
printf 'READY pane=%s\n' "$VVMUX_PANE_ID"
while IFS= read -r line; do printf 'ECHO:%s\n' "$line"; done
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

    let package = directory.path().join("plugin");
    fs::create_dir(&package).unwrap();
    fs::create_dir(package.join("schemas")).unwrap();
    fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/native_host_call_plugin.py"),
        package.join("plugin.py"),
    )
    .unwrap();
    fs::write(
        package.join("vvmux-plugin.toml"),
        r#"manifest_version = 1
[plugin]
id = "dev.broker"
name = "Broker fixture"
version = "1.0.0"
min_vvmux_version = "0.4.0"
description = "Exercises brokered host calls"
platforms = ["linux", "macos"]
permissions = ["session.read", "pane.read", "pane.input"]
[runtime]
kind = "process"
command = ["python3", "plugin.py"]
activation = "on_demand"
[[actions]]
id = "exercise"
title = "Exercise broker"
description = "Call back into the owning session"
handler = "exercise"
input_schema = "schemas/input.json"
output_schema = "schemas/output.json"
timeout_ms = 5000
[[actions]]
id = "slow"
title = "Slow action"
description = "Wait until the supervisor cancels this action"
handler = "slow"
input_schema = "schemas/input.json"
output_schema = "schemas/output.json"
timeout_ms = 30000
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
        r#"{"type":"object","required":["saw_ready","input_accepted","session","broker_token_present","close_denied"]}"#,
    )
    .unwrap();

    assert_success(
        &command(binary, &runtime, &config_home)
            .args(["plugin", "link", package.to_str().unwrap(), "--yes"])
            .output()
            .unwrap(),
    );
    let name = format!("plugin-host-calls-{}", std::process::id());
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
    assert_success(
        &command(binary, &runtime, &config_home)
            .args([
                "msg",
                "--target",
                &name,
                "wait",
                "text",
                "READY",
                "--pane-id",
                "1",
                "--timeout",
                "5s",
            ])
            .output()
            .unwrap(),
    );

    let invoked = command(binary, &runtime, &config_home)
        .args(["plugin", "invoke", "dev.broker/exercise", "--target", &name])
        .output()
        .unwrap();
    assert_success(&invoked);
    let result: Value = serde_json::from_slice(&invoked.stdout).unwrap();
    assert_eq!(result["saw_ready"], true);
    assert_eq!(result["input_accepted"], true);
    assert_eq!(result["session"], name);
    assert_eq!(result["broker_token_present"], true);
    assert_eq!(result["close_denied"], true);

    assert_success(
        &command(binary, &runtime, &config_home)
            .args([
                "msg",
                "--target",
                &name,
                "wait",
                "text",
                "ECHO:BROKER_INPUT",
                "--pane-id",
                "1",
                "--timeout",
                "5s",
            ])
            .output()
            .unwrap(),
    );

    let detached = command(binary, &runtime, &config_home)
        .args([
            "plugin",
            "invoke",
            "dev.broker/exercise",
            "--target",
            &name,
            "--detach",
        ])
        .output()
        .unwrap();
    assert_success(&detached);
    let queued: Value = serde_json::from_slice(&detached.stdout).unwrap();
    let job_id = queued["job_id"].as_str().unwrap();
    assert!(job_id.starts_with(&format!("{name}/")));
    let completed = wait_for_job(
        binary,
        &runtime,
        &config_home,
        job_id,
        "succeeded",
        Duration::from_secs(8),
    );
    assert_eq!(completed["result"]["session"], name);

    let logs = command(binary, &runtime, &config_home)
        .args(["plugin", "job", "logs", job_id])
        .output()
        .unwrap();
    assert_success(&logs);
    let logs: Value = serde_json::from_slice(&logs.stdout).unwrap();
    assert_eq!(logs["status"], "succeeded");
    assert!(logs["stdout"].as_str().unwrap().contains("saw_ready"));
    assert_eq!(logs["stdout_truncated"], false);

    let slow = command(binary, &runtime, &config_home)
        .args([
            "plugin",
            "invoke",
            "dev.broker/slow",
            "--target",
            &name,
            "--detach",
        ])
        .output()
        .unwrap();
    assert_success(&slow);
    let slow: Value = serde_json::from_slice(&slow.stdout).unwrap();
    let slow_job = slow["job_id"].as_str().unwrap();
    let cancel = command(binary, &runtime, &config_home)
        .args(["plugin", "job", "cancel", slow_job])
        .output()
        .unwrap();
    assert_success(&cancel);
    let cancel: Value = serde_json::from_slice(&cancel.stdout).unwrap();
    assert_eq!(cancel["status"], "cancelling");
    let cancelled = wait_for_job(
        binary,
        &runtime,
        &config_home,
        slow_job,
        "cancelled",
        Duration::from_secs(8),
    );
    assert_eq!(cancelled["status"], "cancelled");
    let cancel_again = command(binary, &runtime, &config_home)
        .args(["plugin", "job", "cancel", slow_job])
        .output()
        .unwrap();
    assert_success(&cancel_again);
    let cancel_again: Value = serde_json::from_slice(&cancel_again.stdout).unwrap();
    assert_eq!(cancel_again["status"], "cancelled");
}

fn wait_for_job(
    binary: &str,
    runtime: &Path,
    config_home: &Path,
    job_id: &str,
    expected: &str,
    timeout: Duration,
) -> Value {
    let deadline = Instant::now() + timeout;
    loop {
        let output = command(binary, runtime, config_home)
            .args(["plugin", "job", "status", job_id])
            .output()
            .unwrap();
        assert_success(&output);
        let status: Value = serde_json::from_slice(&output.stdout).unwrap();
        if status["status"] == expected {
            return status;
        }
        assert!(
            Instant::now() < deadline,
            "job {job_id} stayed in state {}",
            status["status"]
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
        "status={} stdout={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
