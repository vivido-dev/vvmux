#![cfg(unix)]

use std::fs;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;
use sha2::{Digest, Sha256};

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
fn rust_component_runs_the_public_abi_with_bounded_authority_and_recovery() {
    let artifact = build_component_fixture();
    let binary = env!("CARGO_BIN_EXE_vvmux");
    let directory = tempfile::Builder::new()
        .prefix("vvc-")
        .tempdir_in("/tmp")
        .unwrap();
    let runtime = directory.path().join("runtime");
    let config_home = directory.path().join("config");
    private_directory(&runtime);
    private_directory(&config_home);
    let config = write_config(directory.path());
    let first_package = write_package(
        directory.path(),
        "one",
        "dev.component.one",
        &artifact,
        true,
    );
    let second_package = write_package(
        directory.path(),
        "two",
        "dev.component.two",
        &artifact,
        false,
    );
    for package in [&first_package, &second_package] {
        assert_success(
            &command(binary, &runtime, &config_home)
                .args(["plugin", "link", package.to_str().unwrap(), "--yes"])
                .output()
                .unwrap(),
        );
    }
    let config_dir = component_plugin_dir(&config_home, "config", "dev.component.one");
    fs::create_dir_all(&config_dir).unwrap();
    fs::write(config_dir.join("config.txt"), b"declared-config").unwrap();

    let name = format!("plugin-component-conformance-{}", std::process::id());
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

    let echo = invoke(
        binary,
        &runtime,
        &config_home,
        &name,
        "dev.component.one/echo",
        r#"{"message":"hello"}"#,
        false,
    );
    assert_eq!(echo["initialized"], true);
    assert_eq!(echo["input"]["message"], "hello");
    assert_eq!(echo["context"]["session"], name);

    let inspected = invoke(
        binary,
        &runtime,
        &config_home,
        &name,
        "dev.component.one/inspect",
        "{}",
        false,
    );
    assert_eq!(inspected["session"], name);
    assert_eq!(
        inspected["session_instance"],
        echo["context"]["session_instance"]
    );

    assert_eq!(
        invoke(
            binary,
            &runtime,
            &config_home,
            &name,
            "dev.component.one/storage",
            r#"{"key":"shared","value":"one"}"#,
            false,
        )["value"],
        "one"
    );
    assert_eq!(
        invoke(
            binary,
            &runtime,
            &config_home,
            &name,
            "dev.component.two/storage",
            r#"{"key":"shared","value":"two"}"#,
            false,
        )["value"],
        "two"
    );
    assert_eq!(
        invoke(
            binary,
            &runtime,
            &config_home,
            &name,
            "dev.component.one/storage",
            r#"{"key":"shared"}"#,
            false,
        )["value"],
        "one",
        "durable keys must be scoped by complete plugin identity"
    );

    let preopens = invoke(
        binary,
        &runtime,
        &config_home,
        &name,
        "dev.component.one/preopens",
        r#"{"probe":true}"#,
        false,
    );
    assert_eq!(preopens["package"], "package-one");
    assert_eq!(preopens["config"], "declared-config");
    assert_eq!(preopens["config_write_denied"], true);
    assert_eq!(preopens["data_write_succeeded"], true);
    assert_eq!(preopens["data_write"], "yes");
    assert_eq!(preopens["undeclared_denied"], true);
    assert_eq!(preopens["undeclared_write_denied"], true);

    let undeclared_preopens = invoke(
        binary,
        &runtime,
        &config_home,
        &name,
        "dev.component.two/preopens",
        "{}",
        false,
    );
    assert!(undeclared_preopens["package"].is_null());
    assert!(undeclared_preopens["config"].is_null());
    assert_eq!(undeclared_preopens["data_write_succeeded"], false);
    assert!(undeclared_preopens["data_write"].is_null());

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let ambient = invoke(
        binary,
        &runtime,
        &config_home,
        &name,
        "dev.component.one/ambient",
        &serde_json::json!({"tcp": address.to_string()}).to_string(),
        false,
    );
    assert_eq!(ambient["environment"], serde_json::json!([]));
    for denial in ["tcp_denied", "udp_denied", "dns_denied", "process_denied"] {
        assert_eq!(ambient[denial], true, "{denial} must remain denied");
    }
    assert!(
        listener.accept().is_err(),
        "the Component must not reach the listener"
    );

    let detached = invoke(
        binary,
        &runtime,
        &config_home,
        &name,
        "dev.component.one/echo",
        r#"{"detached":true}"#,
        true,
    );
    let job_id = detached["job_id"].as_str().unwrap();
    wait_for_job(
        binary,
        &runtime,
        &config_home,
        job_id,
        "succeeded",
        Duration::from_secs(10),
    );
    let logs = job_command(binary, &runtime, &config_home, "logs", job_id);
    assert_eq!(logs["stderr_truncated"], false);
    let entries = logs["stderr"]
        .as_str()
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert!(entries.iter().any(|entry| {
        entry["runtime"] == "component"
            && entry["level"] == "info"
            && entry["message"] == "handled echo"
    }));

    let flooded = invoke(
        binary,
        &runtime,
        &config_home,
        &name,
        "dev.component.one/log-flood",
        "{}",
        true,
    );
    let flooded_job = flooded["job_id"].as_str().unwrap();
    wait_for_job(
        binary,
        &runtime,
        &config_home,
        flooded_job,
        "succeeded",
        Duration::from_secs(10),
    );
    let flooded_logs = job_command(binary, &runtime, &config_home, "logs", flooded_job);
    assert_eq!(flooded_logs["stderr_truncated"], true);
    assert!(flooded_logs["stderr"].as_str().unwrap().len() <= 256 * 1024);
    let first_log: Value = serde_json::from_str(
        flooded_logs["stderr"]
            .as_str()
            .unwrap()
            .lines()
            .next()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(first_log["level"], "warn");
    assert_eq!(first_log["truncated"], true);

    let spinning = invoke(
        binary,
        &runtime,
        &config_home,
        &name,
        "dev.component.one/spin",
        "{}",
        true,
    );
    let spinning_job = spinning["job_id"].as_str().unwrap();
    let cancelled = job_command(binary, &runtime, &config_home, "cancel", spinning_job);
    assert!(matches!(
        cancelled["status"].as_str(),
        Some("cancelling" | "cancelled")
    ));
    wait_for_job(
        binary,
        &runtime,
        &config_home,
        spinning_job,
        "cancelled",
        Duration::from_secs(10),
    );

    thread::sleep(Duration::from_millis(150));
    let trapped = command(binary, &runtime, &config_home)
        .args([
            "plugin",
            "invoke",
            "dev.component.one/trap",
            "--target",
            &name,
        ])
        .output()
        .unwrap();
    assert!(!trapped.status.success());
    assert!(String::from_utf8_lossy(&trapped.stderr).contains("runtime_crashed"));
    thread::sleep(Duration::from_millis(300));
    assert_eq!(
        invoke(
            binary,
            &runtime,
            &config_home,
            &name,
            "dev.component.one/storage",
            r#"{"key":"shared"}"#,
            false,
        )["value"],
        "one",
        "a restarted Component must retain only its own durable storage"
    );

    assert_success(
        &command(binary, &runtime, &config_home)
            .args(["plugin", "disable", "dev.component.one"])
            .output()
            .unwrap(),
    );
    let shutdown = component_storage_path(&config_home, "dev.component.one", "shutdown");
    assert_eq!(fs::read(&shutdown).unwrap(), b"called");

    let cache = only_component_cache(&config_home);
    fs::write(&cache, b"untrusted serialized cache").unwrap();
    assert_success(
        &command(binary, &runtime, &config_home)
            .args(["plugin", "enable", "dev.component.one"])
            .output()
            .unwrap(),
    );
    assert_eq!(
        invoke(
            binary,
            &runtime,
            &config_home,
            &name,
            "dev.component.one/echo",
            "{}",
            false,
        )["initialized"],
        true,
        "an invalid serialized cache must be rejected and rebuilt from the pinned artifact"
    );
    assert!(fs::metadata(cache).unwrap().len() > 1024);
}

fn build_component_fixture() -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let manifest = root.join("tests/fixtures/component_guest/Cargo.toml");
    let target = root.join("target/component-conformance-fixture");
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = Command::new(cargo)
        .current_dir(root)
        .env("CARGO_TARGET_DIR", &target)
        .args([
            "build",
            "--manifest-path",
            manifest.to_str().unwrap(),
            "--target",
            "wasm32-wasip2",
            "--release",
            "--locked",
        ])
        .output()
        .unwrap();
    assert_success(&output);
    let artifact = target
        .join("wasm32-wasip2/release")
        .join("vvmux_component_conformance_fixture.wasm");
    assert!(artifact.is_file());
    artifact
}

fn write_config(root: &Path) -> PathBuf {
    let shell = root.join("fixture-shell");
    fs::write(&shell, b"#!/bin/sh\nwhile :; do sleep 60; done\n").unwrap();
    fs::set_permissions(&shell, fs::Permissions::from_mode(0o700)).unwrap();
    let config = root.join("vvmux.toml");
    fs::write(
        &config,
        format!(
            "[general]\nshell = {}\nrender_interval_ms = 1\n",
            serde_json::to_string(shell.to_str().unwrap()).unwrap()
        ),
    )
    .unwrap();
    config
}

fn write_package(
    root: &Path,
    suffix: &str,
    id: &str,
    artifact: &Path,
    declared_preopens: bool,
) -> PathBuf {
    let package = root.join(format!("plugin-{suffix}"));
    fs::create_dir(&package).unwrap();
    fs::create_dir(package.join("schemas")).unwrap();
    fs::copy(artifact, package.join("plugin.wasm")).unwrap();
    fs::write(package.join("fixture.txt"), format!("package-{suffix}")).unwrap();
    let actions = [
        ("echo", "echo", 5_000),
        ("inspect", "inspect", 5_000),
        ("storage", "storage", 5_000),
        ("preopens", "preopens", 5_000),
        ("ambient", "ambient", 5_000),
        ("log-flood", "log-flood", 5_000),
        ("trap", "trap", 5_000),
        ("spin", "spin", 30_000),
    ]
    .into_iter()
    .map(|(action, handler, timeout)| {
        format!(
            r#"[[actions]]
id = "{action}"
title = "{action}"
description = "Component conformance {action}"
handler = "{handler}"
input_schema = "schemas/input.json"
output_schema = "schemas/output.json"
agent_visible = true
timeout_ms = {timeout}
"#
        )
    })
    .collect::<Vec<_>>()
    .join("\n");
    let preopens = if declared_preopens {
        r#"["package", "config", "data"]"#
    } else {
        "[]"
    };
    fs::write(
        package.join("vvmux-plugin.toml"),
        format!(
            r#"manifest_version = 1
[plugin]
id = "{id}"
name = "Component {suffix}"
version = "1.0.0"
min_vvmux_version = "0.4.0"
description = "Real Rust Component conformance fixture"
platforms = ["linux", "macos"]
permissions = ["session.read"]
[runtime]
kind = "component"
artifact = "plugin.wasm"
activation = "on_demand"
preopens = {preopens}
{actions}
"#
        ),
    )
    .unwrap();
    fs::write(package.join("schemas/input.json"), "{}").unwrap();
    fs::write(package.join("schemas/output.json"), "{}").unwrap();
    package
}

fn invoke(
    binary: &str,
    runtime: &Path,
    config_home: &Path,
    session: &str,
    reference: &str,
    input: &str,
    detach: bool,
) -> Value {
    let mut process = command(binary, runtime, config_home);
    process.args([
        "plugin", "invoke", reference, "--target", session, "--input", input,
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
        let status = job_command(binary, runtime, config_home, "status", job_id);
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

fn job_command(
    binary: &str,
    runtime: &Path,
    config_home: &Path,
    operation: &str,
    job_id: &str,
) -> Value {
    let output = command(binary, runtime, config_home)
        .args(["plugin", "job", operation, job_id])
        .output()
        .unwrap();
    assert_success(&output);
    serde_json::from_slice(&output.stdout).unwrap()
}

fn component_storage_path(config_home: &Path, plugin_id: &str, key: &str) -> PathBuf {
    let key = hex(&Sha256::digest(key.as_bytes()));
    component_plugin_dir(config_home, "data", plugin_id)
        .join("storage")
        .join(format!("s-{key}"))
}

fn component_plugin_dir(config_home: &Path, kind: &str, plugin_id: &str) -> PathBuf {
    let plugin = hex(&Sha256::digest(plugin_id.as_bytes()))[..32].to_owned();
    config_home
        .join(format!("vvmux/plugins/{kind}"))
        .join(format!("p-{plugin}"))
}

fn only_component_cache(config_home: &Path) -> PathBuf {
    let cache = config_home.join("vvmux/plugins/cache/components");
    let entries = fs::read_dir(cache)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "cwasm")
        })
        .collect::<Vec<_>>();
    assert_eq!(
        entries.len(),
        1,
        "identical artifacts must share one cache key"
    );
    entries.into_iter().next().unwrap()
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        result.push(DIGITS[(byte >> 4) as usize] as char);
        result.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    result
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
