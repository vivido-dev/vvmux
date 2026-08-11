#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

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
fn catalog_uses_the_live_session_generation_and_hides_unimplemented_workflows() {
    let binary = env!("CARGO_BIN_EXE_vvmux");
    let directory = tempfile::tempdir().unwrap();
    let runtime = directory.path().join("runtime");
    let config_home = directory.path().join("config");
    private_directory(&runtime);
    private_directory(&config_home);
    let config = write_config(directory.path(), true);
    let package = write_package(directory.path());

    assert_success(
        &command(binary, &runtime, &config_home)
            .args(["plugin", "link", package.to_str().unwrap(), "--yes"])
            .output()
            .unwrap(),
    );
    let name = format!("plugin-contract-catalog-{}", std::process::id());
    start_session(binary, &runtime, &config_home, &config, &name);
    let _guard = SessionGuard {
        binary,
        name: name.clone(),
        runtime: runtime.clone(),
        config_home: config_home.clone(),
    };

    let catalog = plugin_catalog(binary, &runtime, &config_home, &name);
    assert_eq!(catalog["target"], name);
    assert!(catalog["generation"].as_u64().unwrap() > 0);
    let session_instance = catalog["session_instance"].as_str().unwrap();
    assert_eq!(session_instance.len(), 32);
    assert!(
        session_instance
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    );
    let actions = catalog["actions"].as_array().unwrap();
    assert_eq!(actions.len(), 1, "workflows must stay out of discovery");
    assert_eq!(actions[0]["reference"], "dev.contract/echo");
    assert!(actions[0].get("input_schema").is_some());
    assert!(actions[0].get("output_schema").is_some());

    let capabilities = msg_json(binary, &runtime, &config_home, &name, &["capabilities"]);
    assert_eq!(capabilities["plugins"]["enabled"], true);
    assert_eq!(
        capabilities["plugins"]["session_instance"],
        catalog["session_instance"]
    );
    assert_eq!(
        capabilities["plugins"]["applied_generation"],
        catalog["generation"]
    );
    assert_eq!(
        capabilities["plugins"]["enforceable_capabilities"],
        serde_json::json!([
            "session.read",
            "pane.read",
            "pane.input",
            "pane.create",
            "pane.manage_own",
            "pane.manage_any",
            "events.subscribe",
            "plugin.invoke",
            "media.produce"
        ])
    );

    let invoked = command(binary, &runtime, &config_home)
        .args(["plugin", "invoke", "dev.contract/echo", "--target", &name])
        .output()
        .unwrap();
    assert_success(&invoked);
    let invoked: Value = serde_json::from_slice(&invoked.stdout).unwrap();
    assert_eq!(invoked, serde_json::json!({"broker_token": false}));
}

#[test]
fn global_kill_switch_rejects_work_and_can_enable_and_disable_live() {
    let binary = env!("CARGO_BIN_EXE_vvmux");
    let directory = tempfile::tempdir().unwrap();
    let runtime = directory.path().join("runtime");
    let config_home = directory.path().join("config");
    private_directory(&runtime);
    private_directory(&config_home);
    let config = write_config(directory.path(), false);
    let package = write_package(directory.path());
    assert_success(
        &command(binary, &runtime, &config_home)
            .args(["plugin", "link", package.to_str().unwrap(), "--yes"])
            .output()
            .unwrap(),
    );

    let name = format!("plugin-contract-switch-{}", std::process::id());
    start_session(binary, &runtime, &config_home, &config, &name);
    let _guard = SessionGuard {
        binary,
        name: name.clone(),
        runtime: runtime.clone(),
        config_home: config_home.clone(),
    };

    let capabilities = msg_json(binary, &runtime, &config_home, &name, &["capabilities"]);
    assert_eq!(capabilities["plugins"]["enabled"], false);
    assert!(capabilities["plugins"]["applied_generation"].is_null());
    assert!(
        capabilities["plugins"]["actions"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_plugin_error(binary, &runtime, &config_home, &name, "plugin_disabled");
    let catalog = command(binary, &runtime, &config_home)
        .args(["plugin", "catalog", "--target", &name, "--json"])
        .output()
        .unwrap();
    assert!(!catalog.status.success());
    assert!(String::from_utf8_lossy(&catalog.stderr).contains("plugin_disabled"));

    // A session with the global switch off has no applied runtime generation and therefore must
    // not block atomic user-registry maintenance for other/future sessions.
    assert_success(
        &command(binary, &runtime, &config_home)
            .args(["plugin", "disable", "dev.contract"])
            .output()
            .unwrap(),
    );
    assert_success(
        &command(binary, &runtime, &config_home)
            .args(["plugin", "enable", "dev.contract"])
            .output()
            .unwrap(),
    );

    rewrite_config(&config, true);
    let enabled = msg_json(binary, &runtime, &config_home, &name, &["reload-config"]);
    assert!(
        enabled["applied"]
            .as_array()
            .unwrap()
            .iter()
            .any(|section| section == "plugins.enabled")
    );
    let catalog = plugin_catalog(binary, &runtime, &config_home, &name);
    assert_eq!(catalog["actions"].as_array().unwrap().len(), 1);

    rewrite_config(&config, false);
    let disabled = msg_json(binary, &runtime, &config_home, &name, &["reload-config"]);
    assert!(
        disabled["applied"]
            .as_array()
            .unwrap()
            .iter()
            .any(|section| section == "plugins.enabled")
    );
    assert_plugin_error(binary, &runtime, &config_home, &name, "plugin_disabled");
}

#[test]
fn disabled_and_enabled_empty_sessions_render_identical_terminal_grids() {
    let binary = env!("CARGO_BIN_EXE_vvmux");
    let directory = tempfile::tempdir().unwrap();
    let runtime = directory.path().join("runtime");
    let config_home = directory.path().join("config");
    private_directory(&runtime);
    private_directory(&config_home);
    let disabled_config = write_named_config(directory.path(), "disabled.toml", false);
    let enabled_config = write_named_config(directory.path(), "enabled.toml", true);
    let suffix = std::process::id();
    let disabled_name = format!("plugin-contract-grid-off-{suffix}");
    let enabled_name = format!("plugin-contract-grid-empty-{suffix}");
    start_session(
        binary,
        &runtime,
        &config_home,
        &disabled_config,
        &disabled_name,
    );
    let _disabled_guard = SessionGuard {
        binary,
        name: disabled_name.clone(),
        runtime: runtime.clone(),
        config_home: config_home.clone(),
    };
    start_session(
        binary,
        &runtime,
        &config_home,
        &enabled_config,
        &enabled_name,
    );
    let _enabled_guard = SessionGuard {
        binary,
        name: enabled_name.clone(),
        runtime: runtime.clone(),
        config_home: config_home.clone(),
    };

    wait_text(binary, &runtime, &config_home, &disabled_name, "READY");
    wait_text(binary, &runtime, &config_home, &enabled_name, "READY");
    let disabled = msg_json(
        binary,
        &runtime,
        &config_home,
        &disabled_name,
        &["get-grid", "--pane-id", "1"],
    );
    let enabled = msg_json(
        binary,
        &runtime,
        &config_home,
        &enabled_name,
        &["get-grid", "--pane-id", "1"],
    );
    assert_eq!(disabled["grid"], enabled["grid"]);
    assert_eq!(disabled["rows"], enabled["rows"]);

    let disabled_before = msg_json(
        binary,
        &runtime,
        &config_home,
        &disabled_name,
        &["list-panes"],
    )["actor_wakeups"]
        .as_u64()
        .unwrap();
    let enabled_before = msg_json(
        binary,
        &runtime,
        &config_home,
        &enabled_name,
        &["list-panes"],
    )["actor_wakeups"]
        .as_u64()
        .unwrap();
    let disabled_after = msg_json(
        binary,
        &runtime,
        &config_home,
        &disabled_name,
        &["list-panes"],
    )["actor_wakeups"]
        .as_u64()
        .unwrap();
    let enabled_after = msg_json(
        binary,
        &runtime,
        &config_home,
        &enabled_name,
        &["list-panes"],
    )["actor_wakeups"]
        .as_u64()
        .unwrap();
    let disabled_delta = disabled_after - disabled_before;
    let enabled_delta = enabled_after - enabled_before;
    assert!(disabled_delta >= 1);
    assert_eq!(enabled_delta, disabled_delta);
}

fn write_package(root: &Path) -> PathBuf {
    let package = root.join("plugin");
    fs::create_dir_all(package.join("schemas")).unwrap();
    let action = package.join("action.sh");
    fs::write(
        &action,
        b"#!/bin/sh\nif test -n \"$VVMUX_PLUGIN_BROKER_TOKEN\"; then printf '{\"broker_token\":true}'; else printf '{\"broker_token\":false}'; fi\n",
    )
    .unwrap();
    fs::set_permissions(&action, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(
        package.join("vvmux-plugin.toml"),
        r#"manifest_version = 1
[plugin]
id = "dev.contract"
name = "Contract fixture"
version = "1.0.0"
min_vvmux_version = "0.4.0"
description = "Session contract fixture"
platforms = ["linux", "macos"]
permissions = ["pane.read"]
[[actions]]
id = "echo"
title = "Echo"
description = "Return one bounded object"
command = ["./action.sh"]
input_schema = "schemas/input.json"
output_schema = "schemas/output.json"
agent_visible = true
timeout_ms = 5000
[[workflows]]
id = "future"
title = "Not executable yet"
agent_visible = true
output = {}
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
        r#"{"type":"object","required":["broker_token"],"properties":{"broker_token":{"type":"boolean"}},"additionalProperties":false}"#,
    )
    .unwrap();
    package
}

fn write_config(root: &Path, plugins_enabled: bool) -> PathBuf {
    write_named_config(root, "vvmux.toml", plugins_enabled)
}

fn write_named_config(root: &Path, name: &str, plugins_enabled: bool) -> PathBuf {
    let shell = root.join("fixture-shell");
    if !shell.exists() {
        fs::write(
            &shell,
            b"#!/bin/sh\nprintf 'READY\\n'\nwhile :; do sleep 60; done\n",
        )
        .unwrap();
        fs::set_permissions(&shell, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let config = root.join(name);
    fs::write(
        &config,
        format!(
            "[general]\nshell = {}\nrender_interval_ms = 1\n\n[plugins]\nenabled = {plugins_enabled}\n",
            serde_json::to_string(shell.to_str().unwrap()).unwrap()
        ),
    )
    .unwrap();
    config
}

fn rewrite_config(path: &Path, plugins_enabled: bool) {
    let source = fs::read_to_string(path).unwrap();
    let source = if plugins_enabled {
        source.replace("enabled = false", "enabled = true")
    } else {
        source.replace("enabled = true", "enabled = false")
    };
    fs::write(path, source).unwrap();
}

fn start_session(binary: &str, runtime: &Path, config_home: &Path, config: &Path, name: &str) {
    let output = command(binary, runtime, config_home)
        .args([
            "--config",
            config.to_str().unwrap(),
            "new",
            "-s",
            name,
            "-d",
        ])
        .output()
        .unwrap();
    assert_success(&output);
}

fn plugin_catalog(binary: &str, runtime: &Path, config_home: &Path, session: &str) -> Value {
    let output = command(binary, runtime, config_home)
        .args(["plugin", "catalog", "--target", session, "--json"])
        .output()
        .unwrap();
    assert_success(&output);
    serde_json::from_slice(&output.stdout).unwrap()
}

fn assert_plugin_error(
    binary: &str,
    runtime: &Path,
    config_home: &Path,
    session: &str,
    expected: &str,
) {
    let output = command(binary, runtime, config_home)
        .args(["plugin", "invoke", "dev.contract/echo", "--target", session])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains(expected));
}

fn wait_text(binary: &str, runtime: &Path, config_home: &Path, session: &str, text: &str) {
    let output = command(binary, runtime, config_home)
        .args([
            "msg",
            "--target",
            session,
            "wait",
            "text",
            text,
            "--pane-id",
            "1",
            "--timeout",
            "5s",
        ])
        .output()
        .unwrap();
    assert_success(&output);
}

fn msg_json(
    binary: &str,
    runtime: &Path,
    config_home: &Path,
    session: &str,
    arguments: &[&str],
) -> Value {
    let mut process = command(binary, runtime, config_home);
    process.args(["msg", "--target", session]).args(arguments);
    let output = process.output().unwrap();
    assert_success(&output);
    serde_json::from_slice(&output.stdout).unwrap()
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
