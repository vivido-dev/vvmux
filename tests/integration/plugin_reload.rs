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
fn registry_generations_drain_restart_disable_and_watch_live_sessions() {
    let binary = env!("CARGO_BIN_EXE_vvmux");
    let directory = tempfile::Builder::new()
        .prefix("vpr-")
        .tempdir_in("/tmp")
        .unwrap();
    let runtime = directory.path().join("runtime");
    let config_home = directory.path().join("config");
    private_directory(&runtime);
    private_directory(&config_home);

    let shell = directory.path().join("fixture-shell");
    fs::write(&shell, b"#!/bin/sh\nwhile :; do sleep 60; done\n").unwrap();
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
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/native_reload_plugin.py"),
        package.join("plugin.py"),
    )
    .unwrap();
    write_package(&package, "one");
    assert_success(
        &command(binary, &runtime, &config_home)
            .args(["plugin", "install", package.to_str().unwrap(), "--yes"])
            .output()
            .unwrap(),
    );

    let name = format!("plugin-reload-{}", std::process::id());
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
    let second_name = format!("plugin-reload-peer-{}", std::process::id());
    assert_success(
        &command(binary, &runtime, &config_home)
            .args([
                "--config",
                config.to_str().unwrap(),
                "new",
                "-s",
                &second_name,
                "-d",
            ])
            .output()
            .unwrap(),
    );
    let _second_guard = SessionGuard {
        binary,
        name: second_name.clone(),
        runtime: runtime.clone(),
        config_home: config_home.clone(),
    };

    let first = invoke(
        binary,
        &runtime,
        &config_home,
        &name,
        "version",
        "{}",
        false,
    );
    assert_eq!(first["version"], "one");
    let first_instance = first["instance"].as_str().unwrap().to_owned();
    assert_eq!(
        invoke(
            binary,
            &runtime,
            &config_home,
            &second_name,
            "version",
            "{}",
            false
        )["version"],
        "one"
    );

    let slow = invoke(
        binary,
        &runtime,
        &config_home,
        &name,
        "slow",
        r#"{"seconds":1}"#,
        true,
    );
    let slow_job = slow["job_id"].as_str().unwrap().to_owned();
    thread::sleep(Duration::from_millis(150));
    write_package(&package, "two");
    let update_started = Instant::now();
    assert_success(
        &command(binary, &runtime, &config_home)
            .args(["plugin", "update", "dev.reload", "--yes"])
            .output()
            .unwrap(),
    );
    assert!(
        update_started.elapsed() >= Duration::from_millis(500),
        "changed artifacts must drain an active call before update acknowledgement"
    );
    let old_completion = wait_for_job(
        binary,
        &runtime,
        &config_home,
        &slow_job,
        "succeeded",
        Duration::from_secs(5),
    );
    assert_eq!(old_completion["result"]["version"], "one");
    let second = invoke(
        binary,
        &runtime,
        &config_home,
        &name,
        "version",
        "{}",
        false,
    );
    assert_eq!(second["version"], "two");
    assert_ne!(second["instance"], first_instance);
    assert_eq!(
        invoke(
            binary,
            &runtime,
            &config_home,
            &second_name,
            "version",
            "{}",
            false
        )["version"],
        "two",
        "the committed generation must reach every live session"
    );

    let blocked = invoke(
        binary,
        &runtime,
        &config_home,
        &name,
        "slow",
        r#"{"seconds":30}"#,
        true,
    );
    let blocked_job = blocked["job_id"].as_str().unwrap().to_owned();
    thread::sleep(Duration::from_millis(150));
    let disable_started = Instant::now();
    assert_success(
        &command(binary, &runtime, &config_home)
            .args(["plugin", "disable", "dev.reload"])
            .output()
            .unwrap(),
    );
    assert!(disable_started.elapsed() < Duration::from_secs(8));
    let cancelled = wait_for_job(
        binary,
        &runtime,
        &config_home,
        &blocked_job,
        "cancelled",
        Duration::from_secs(5),
    );
    assert_eq!(cancelled["status"], "cancelled");
    assert_invoke_error(binary, &runtime, &config_home, &name, "plugin_disabled");
    assert_invoke_error(
        binary,
        &runtime,
        &config_home,
        &second_name,
        "plugin_disabled",
    );

    assert_success(
        &command(binary, &runtime, &config_home)
            .args(["plugin", "enable", "dev.reload"])
            .output()
            .unwrap(),
    );
    assert_eq!(
        invoke(
            binary,
            &runtime,
            &config_home,
            &name,
            "version",
            "{}",
            false
        )["version"],
        "two"
    );
    assert_eq!(
        invoke(
            binary,
            &runtime,
            &config_home,
            &second_name,
            "version",
            "{}",
            false
        )["version"],
        "two"
    );

    let registry_path = config_home.join("vvmux/plugins/registry.json");
    edit_registry(&registry_path, |registry| {
        registry["generation"] = Value::from(registry["generation"].as_u64().unwrap() + 1);
        registry["plugins"]["dev.reload"]["enabled"] = Value::Bool(false);
    });
    wait_for_invoke_error(
        binary,
        &runtime,
        &config_home,
        &name,
        "plugin_disabled",
        Duration::from_secs(6),
    );
    wait_for_invoke_error(
        binary,
        &runtime,
        &config_home,
        &second_name,
        "plugin_disabled",
        Duration::from_secs(2),
    );
    edit_registry(&registry_path, |registry| {
        registry["generation"] = Value::from(registry["generation"].as_u64().unwrap() + 1);
        registry["plugins"]["dev.reload"]["enabled"] = Value::Bool(true);
    });
    wait_for_version(
        binary,
        &runtime,
        &config_home,
        &name,
        "two",
        Duration::from_secs(6),
    );
    wait_for_version(
        binary,
        &runtime,
        &config_home,
        &second_name,
        "two",
        Duration::from_secs(2),
    );

    let good_registry = fs::read(&registry_path).unwrap();
    let missing_root = config_home.join("vvmux/plugins/packages/p-missing");
    edit_registry(&registry_path, |registry| {
        registry["generation"] = Value::from(registry["generation"].as_u64().unwrap() + 1);
        registry["plugins"]["dev.reload"]["root"] =
            Value::from(missing_root.to_string_lossy().into_owned());
    });
    let reload = command(binary, &runtime, &config_home)
        .args(["msg", "--target", &name, "reload-plugins"])
        .output()
        .unwrap();
    assert_success(&reload);
    let report: Value = serde_json::from_slice(&reload.stdout).unwrap();
    assert!(report["failed"].get("dev.reload").is_some());
    assert_eq!(
        invoke(
            binary,
            &runtime,
            &config_home,
            &name,
            "version",
            "{}",
            false
        )["version"],
        "two",
        "an invalid generation must leave the prior runtime available"
    );
    assert_eq!(
        invoke(
            binary,
            &runtime,
            &config_home,
            &second_name,
            "version",
            "{}",
            false
        )["version"],
        "two"
    );
    let mut restored: Value = serde_json::from_slice(&good_registry).unwrap();
    let broken: Value = serde_json::from_slice(&fs::read(&registry_path).unwrap()).unwrap();
    restored["generation"] = Value::from(broken["generation"].as_u64().unwrap() + 1);
    atomic_json_write(&registry_path, &restored);
    assert_success(
        &command(binary, &runtime, &config_home)
            .args(["msg", "--target", &name, "reload-plugins"])
            .output()
            .unwrap(),
    );

    let current: Value = serde_json::from_slice(&fs::read(&registry_path).unwrap()).unwrap();
    let mut stale = current.clone();
    stale["generation"] = Value::from(current["generation"].as_u64().unwrap() - 1);
    atomic_json_write(&registry_path, &stale);
    let rejected = command(binary, &runtime, &config_home)
        .args(["msg", "--target", &name, "reload-plugins"])
        .output()
        .unwrap();
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("older than applied generation"));
    let mut newest = current;
    newest["generation"] = Value::from(stale["generation"].as_u64().unwrap() + 2);
    atomic_json_write(&registry_path, &newest);
    assert_success(
        &command(binary, &runtime, &config_home)
            .args(["msg", "--target", &name, "reload-plugins"])
            .output()
            .unwrap(),
    );

    let removed = invoke(
        binary,
        &runtime,
        &config_home,
        &name,
        "slow",
        r#"{"seconds":30}"#,
        true,
    );
    let removed_job = removed["job_id"].as_str().unwrap().to_owned();
    thread::sleep(Duration::from_millis(150));
    assert_success(
        &command(binary, &runtime, &config_home)
            .args(["plugin", "uninstall", "dev.reload"])
            .output()
            .unwrap(),
    );
    assert_eq!(
        wait_for_job(
            binary,
            &runtime,
            &config_home,
            &removed_job,
            "cancelled",
            Duration::from_secs(5),
        )["status"],
        "cancelled"
    );
    let not_found = command(binary, &runtime, &config_home)
        .args(["plugin", "invoke", "dev.reload/version", "--target", &name])
        .output()
        .unwrap();
    assert!(!not_found.status.success());
    assert!(String::from_utf8_lossy(&not_found.stderr).contains("plugin_not_found"));
}

fn write_package(package: &Path, version: &str) {
    fs::write(package.join("VERSION"), version).unwrap();
    fs::write(
        package.join("vvmux-plugin.toml"),
        r#"manifest_version = 1
[plugin]
id = "dev.reload"
name = "Reload fixture"
version = "1.0.0"
min_vvmux_version = "0.4.0"
description = "Exercises registry generations"
platforms = ["linux", "macos"]
permissions = []
[runtime]
kind = "process"
command = ["python3", "plugin.py"]
activation = "on_demand"
[[actions]]
id = "version"
title = "Version"
description = "Return the loaded artifact version"
handler = "version"
input_schema = "schemas/input.json"
output_schema = "schemas/output.json"
timeout_ms = 5000
[[actions]]
id = "slow"
title = "Slow"
description = "Stay active across a reload"
handler = "slow"
input_schema = "schemas/slow-input.json"
output_schema = "schemas/output.json"
timeout_ms = 35000
"#,
    )
    .unwrap();
    fs::write(
        package.join("schemas/input.json"),
        r#"{"type":"object","additionalProperties":false}"#,
    )
    .unwrap();
    fs::write(
        package.join("schemas/slow-input.json"),
        r#"{"type":"object","properties":{"seconds":{"type":"number","minimum":0}},"additionalProperties":false}"#,
    )
    .unwrap();
    fs::write(
        package.join("schemas/output.json"),
        r#"{"type":"object","required":["version","instance"],"properties":{"version":{"type":"string"},"instance":{"type":"string"}},"additionalProperties":false}"#,
    )
    .unwrap();
}

fn invoke(
    binary: &str,
    runtime: &Path,
    config_home: &Path,
    session: &str,
    action: &str,
    input: &str,
    detach: bool,
) -> Value {
    let mut process = command(binary, runtime, config_home);
    process.args([
        "plugin",
        "invoke",
        &format!("dev.reload/{action}"),
        "--target",
        session,
        "--input",
        input,
    ]);
    if detach {
        process.arg("--detach");
    }
    let output = process.output().unwrap();
    assert_success(&output);
    serde_json::from_slice(&output.stdout).unwrap()
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
        assert!(Instant::now() < deadline, "job stayed in {status}");
        thread::sleep(Duration::from_millis(25));
    }
}

fn assert_invoke_error(
    binary: &str,
    runtime: &Path,
    config_home: &Path,
    session: &str,
    expected: &str,
) {
    let output = command(binary, runtime, config_home)
        .args([
            "plugin",
            "invoke",
            "dev.reload/version",
            "--target",
            session,
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains(expected));
}

fn wait_for_invoke_error(
    binary: &str,
    runtime: &Path,
    config_home: &Path,
    session: &str,
    expected: &str,
    timeout: Duration,
) {
    let deadline = Instant::now() + timeout;
    loop {
        let output = command(binary, runtime, config_home)
            .args([
                "plugin",
                "invoke",
                "dev.reload/version",
                "--target",
                session,
            ])
            .output()
            .unwrap();
        if !output.status.success() && String::from_utf8_lossy(&output.stderr).contains(expected) {
            return;
        }
        assert!(Instant::now() < deadline, "watcher did not apply disable");
        thread::sleep(Duration::from_millis(100));
    }
}

fn wait_for_version(
    binary: &str,
    runtime: &Path,
    config_home: &Path,
    session: &str,
    expected: &str,
    timeout: Duration,
) {
    let deadline = Instant::now() + timeout;
    loop {
        let output = command(binary, runtime, config_home)
            .args([
                "plugin",
                "invoke",
                "dev.reload/version",
                "--target",
                session,
            ])
            .output()
            .unwrap();
        if output.status.success()
            && serde_json::from_slice::<Value>(&output.stdout).unwrap()["version"] == expected
        {
            return;
        }
        assert!(Instant::now() < deadline, "watcher did not apply enable");
        thread::sleep(Duration::from_millis(100));
    }
}

fn edit_registry(path: &Path, edit: impl FnOnce(&mut Value)) {
    let mut registry: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    edit(&mut registry);
    atomic_json_write(path, &registry);
}

fn atomic_json_write(path: &Path, value: &Value) {
    let temporary = path.with_extension("test.tmp");
    fs::write(&temporary, serde_json::to_vec_pretty(value).unwrap()).unwrap();
    fs::rename(temporary, path).unwrap();
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
